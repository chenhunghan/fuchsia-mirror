#!/usr/bin/env fuchsia-vendored-python
# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit-tests for build/build_tests_json.py functions."""

import json
import os
import sys
import tempfile
import typing as T
import unittest
from pathlib import Path

_SCRIPT_DIR = os.path.dirname(__file__)
sys.path.insert(0, _SCRIPT_DIR)
import build_tests_json
from build_utils import BazelPaths, CommandRunner, MockCommandRunner
from serialization import instance_from_dict, instance_to_dict


class BuildTestsJsonTest(unittest.TestCase):
    def setUp(self) -> None:
        self._td = tempfile.TemporaryDirectory()
        self._dir = Path(self._td.name)
        self.source_dir = self._dir / "source"
        self.source_dir.mkdir()
        (self.source_dir / ".jiri_manifest").touch()

        self.build_dir = self.source_dir / "out" / "not-default"
        self.build_dir.mkdir(parents=True)

        self.output_dir = self._dir / "output"
        self.output_dir.mkdir()

        (self.build_dir / "obj" / "tests").mkdir(parents=True)

        # Compute the Bazel execroot path, relative to the build directory.
        BazelPaths.write_topdir_config_for_test(self.source_dir, "bazel_topdir")
        self.execroot_path = os.path.relpath(
            BazelPaths(self.source_dir, self.build_dir).execroot,
            self.build_dir,
        )

    def tearDown(self) -> None:
        self._td.cleanup()

    def _test(
        self,
        tests_from_metadata: list[T.Any],
        test_groups: list[T.Any],
        product_bundles: list[T.Any],
        with_bazel_tests: bool = False,
        command_runner: CommandRunner | None = None,
        all_test_environments: dict[str, T.Any] | None = None,
        default_test_environments: dict[str, T.Any] | None = None,
    ) -> tuple[set[Path], list[T.Any]]:
        tests_from_metadata_str = json.dumps(tests_from_metadata)
        tests_from_metadata_path = self.build_dir / "tests_from_metadata.json"
        tests_from_metadata_path.write_text(tests_from_metadata_str)

        test_groups_str = json.dumps(test_groups)
        test_groups_path = (
            self.build_dir / "obj" / "tests" / "product_bundle_test_groups.json"
        )
        test_groups_path.write_text(test_groups_str)

        if all_test_environments is None:
            all_test_environments = {
                "all_environments": [],
                "platforms": [
                    {"device_type": "AEMU", "cpu": "x64"},
                    {"device_type": "QEMU", "cpu": "x64"},
                    {"device_type": "QEMU", "cpu": "arm64"},
                    {
                        "device_type": "QEMU",
                        "cpu": "arm64",
                        "host_device_type": "GCP_C4A_HIGHMEM_96_BM",
                    },
                    {
                        "device_type": "QEMU",
                        "cpu": "arm64",
                        "host_device_type": "AmpereAltraMax-M128-30",
                    },
                    {"device_type": "Vim3", "cpu": "arm64"},
                    {"device_type": "Astro", "cpu": "arm64"},
                    {"os": "Linux", "cpu": "x64"},
                    {"os": "Linux", "cpu": "arm64"},
                ],
                "all_dimension_keys": [
                    "device_type",
                    "cpu",
                    "os",
                    "pool",
                    "testbed",
                    "host_device_type",
                ],
                "host_env": {"dimensions": {"os": "Linux", "cpu": "x64"}},
            }
        environments_path = (
            self.build_dir / "obj" / "tests" / "all_test_environments.json"
        )
        environments_path.write_text(json.dumps(all_test_environments))

        if default_test_environments is None:
            default_test_environments = {
                "target_cpu": "x64",
                "default_environments": [
                    {"dimensions": {"device_type": "AEMU"}}
                ],
                "allowed_device_types": ["AEMU", "QEMU"],
                "allowed_host_device_types": [],
            }
        default_environments_path = (
            self.build_dir / "obj" / "tests" / "default_test_environments.json"
        )
        default_environments_path.write_text(
            json.dumps(default_test_environments)
        )

        product_bundles_str = json.dumps(product_bundles)
        product_bundles_path = self.build_dir / "product_bundles.json"
        product_bundles_path.write_text(product_bundles_str)

        if with_bazel_tests:
            (self.build_dir / "bazel_host_test_suites.txt").write_text(
                "//fake/test1\n//fake/test2"
            )
        inputs = build_tests_json.build_tests_json(
            self.build_dir, with_bazel_tests, command_runner
        )

        tests_string = (self.build_dir / "tests.json").read_text()
        tests = json.loads(tests_string)

        return (inputs, tests)

    def test_only_tests_from_metadata_no_environments(self) -> None:
        tests_from_metadata = [
            {"test": {"name": "test1"}},
            {"test": {"name": "test2"}},
        ]
        product_bundles = [{"name": "my_pb"}]
        (_, tests) = self._test(tests_from_metadata, [], product_bundles)
        expected_tests = [
            {
                "test": {"name": "test1"},
                "environments": [{"dimensions": {"device_type": "AEMU"}}],
            },
            {
                "test": {"name": "test2"},
                "environments": [{"dimensions": {"device_type": "AEMU"}}],
            },
        ]
        self.assertEqual(expected_tests, tests)

    def test_only_test_groups(self) -> None:
        tests_json = [{"test": {"name": "test1"}}, {"test": {"name": "test2"}}]
        tests_json_str = json.dumps(tests_json)
        tests_json_path = self.build_dir / "pb_tests.json"
        tests_json_path.write_text(tests_json_str)

        test_groups = [
            {"product_bundle_name": "my_pb", "tests_json": str(tests_json_path)}
        ]
        product_bundles = [{"name": "my_pb"}]
        (_, tests) = self._test([], test_groups, product_bundles)

        expected_tests_json = [
            {
                "product_bundle": "my_pb",
                "test": {"name": "test1-my_pb"},
                "build_only": True,
                "environments": [],
            },
            {
                "product_bundle": "my_pb",
                "test": {"name": "test2-my_pb"},
                "build_only": True,
                "environments": [],
            },
        ]
        self.assertEqual(expected_tests_json, tests)

    def test_incorrect_product_bundle_name(self) -> None:
        tests_json = [{"test": {"name": "test1"}}, {"test": {"name": "test2"}}]
        tests_json_str = json.dumps(tests_json)
        tests_json_path = self.build_dir / "pb_tests.json"
        tests_json_path.write_text(tests_json_str)

        test_groups = [
            {
                "product_bundle_name": "my_pb_incorrect",
                "tests_json": str(tests_json_path),
            }
        ]
        product_bundles = [{"name": "my_pb"}]

        with self.assertRaises(SystemExit):
            self._test([], test_groups, product_bundles)

    def test_metadata_and_product_bundles(self) -> None:
        tests_from_metadata = [
            {"test": {"name": "test1"}},
            {"test": {"name": "test2"}},
        ]
        tests_json = [{"test": {"name": "test1"}}, {"test": {"name": "test2"}}]
        tests_json_str = json.dumps(tests_json)
        tests_json_path = self.build_dir / "pb_tests.json"
        tests_json_path.write_text(tests_json_str)
        product_bundles = [{"name": "my_pb"}]

        env = {"dimensions": {"device_type": "Vim3"}}
        test_groups = [
            {
                "product_bundle_name": "my_pb",
                "environments": [env],
                "tests_json": str(tests_json_path),
            }
        ]

        _, tests = self._test(
            tests_from_metadata,
            test_groups,
            product_bundles,
        )

        default_env = {"dimensions": {"device_type": "AEMU"}}
        expected_tests_json = [
            {"test": {"name": "test1"}, "environments": [default_env]},
            {"test": {"name": "test2"}, "environments": [default_env]},
            {
                "product_bundle": "my_pb",
                "environments": [env],
                "test": {"name": "test1-my_pb"},
            },
            {
                "product_bundle": "my_pb",
                "environments": [env],
                "test": {"name": "test2-my_pb"},
            },
        ]
        self.assertEqual(expected_tests_json, tests)

    def test_bazel_tests(self) -> None:
        # Prepare mock runner for bazel cquery
        mock_runner = MockCommandRunner()
        test1 = {
            "name": "test1",
            "label": "@@//t1",
            "source_label": "@@//t1",
            "launcher_execroot_path": "p1",
            "runtime_deps_json_execroot_path": "d1",
            "os": "linux",
            "cpu": "x64",
            "list_cases_argument": "list_cases_1",
        }
        test2 = {
            "name": "test2",
            "label": "@@//t2",
            "source_label": "@@//t2",
            "launcher_execroot_path": "p2",
            "runtime_deps_json_execroot_path": "d2",
            "os": "linux",
            "cpu": "x64",
            "list_cases_argument": "",
        }
        mock_runner.push_result(
            stdout=json.dumps(test1) + "\n" + json.dumps(test2)
        )

        dummy_host_test = {
            "environments": [
                {
                    "dimensions": {
                        "os": "Linux",
                        "cpu": "x64",
                    }
                }
            ],
            "test": {
                "name": "dummy_host_test",
            },
        }
        _, tests = self._test(
            [dummy_host_test],
            [],
            [],
            with_bazel_tests=True,
            command_runner=mock_runner,
        )

        expected_tests_json: list[dict[str, T.Any]] = [
            dummy_host_test,
            {
                "environments": [
                    {
                        "dimensions": {
                            "os": "Linux",
                            "cpu": "x64",
                        }
                    }
                ],
                "expects_ssh": False,
                "test": {
                    "name": "//t1",
                    "label": "@@//t1",
                    "source_label": "//t1",
                    "path": f"{self.execroot_path}/p1",
                    "runtime_deps": f"{self.execroot_path}/d1",
                    "os": "linux",
                    "cpu": "x64",
                    "list_cases_argument": "list_cases_1",
                },
            },
            {
                "environments": [
                    {
                        "dimensions": {
                            "os": "Linux",
                            "cpu": "x64",
                        }
                    }
                ],
                "expects_ssh": False,
                "test": {
                    "name": "//t2",
                    "label": "@@//t2",
                    "source_label": "//t2",
                    "path": f"{self.execroot_path}/p2",
                    "runtime_deps": f"{self.execroot_path}/d2",
                    "os": "linux",
                    "cpu": "x64",
                },
            },
        ]
        self.assertEqual(len(expected_tests_json), len(tests))
        self.assertDictEqual(expected_tests_json[0], tests[0])
        self.assertDictEqual(expected_tests_json[1], tests[1])

    def test_full(self) -> None:
        tests_from_metadata = [
            {
                "test": {"name": "test1"},
                "environments": [{"dimensions": {"os": "Linux"}}],
            },
            {"test": {"name": "test2"}},
        ]
        tests_json = [{"test": {"name": "test1"}}, {"test": {"name": "test2"}}]
        tests_json_str = json.dumps(tests_json)
        tests_json_path = self.build_dir / "pb_tests.json"
        tests_json_path.write_text(tests_json_str)
        product_bundles = [{"name": "my_pb"}]

        env = {"dimensions": {"device_type": "Vim3"}}
        test_groups = [
            {
                "product_bundle_name": "my_pb",
                "environments": [env],
                "tests_json": str(tests_json_path),
            }
        ]

        mock_runner = MockCommandRunner()
        test1 = {
            "name": "test1",
            "label": "@@//t1",
            "source_label": "@@//t1",
            "launcher_execroot_path": "p1",
            "runtime_deps_json_execroot_path": "d1",
            "os": "linux",
            "cpu": "x64",
            "list_cases_argument": "",
        }
        mock_runner.push_result(stdout=json.dumps(test1))

        (_, tests) = self._test(
            tests_from_metadata,
            test_groups,
            product_bundles,
            with_bazel_tests=True,
            command_runner=mock_runner,
        )

        expected_tests_json: list[dict[str, T.Any]] = [
            {
                "test": {"name": "test1"},
                "environments": [{"dimensions": {"os": "Linux"}}],
            },
            {
                "test": {"name": "test2"},
                "environments": [{"dimensions": {"device_type": "AEMU"}}],
            },
            {
                "product_bundle": "my_pb",
                "environments": [env],
                "test": {"name": "test1-my_pb"},
            },
            {
                "product_bundle": "my_pb",
                "environments": [env],
                "test": {"name": "test2-my_pb"},
            },
            {
                "environments": [
                    {
                        "dimensions": {
                            "os": "Linux",
                            "cpu": "x64",
                        }
                    }
                ],
                "expects_ssh": False,
                "test": {
                    "name": "//t1",
                    "label": "@@//t1",
                    "source_label": "//t1",
                    "path": f"{self.execroot_path}/p1",
                    "runtime_deps": f"{self.execroot_path}/d1",
                    "os": "linux",
                    "cpu": "x64",
                },
            },
        ]
        self.assertEqual(expected_tests_json, tests)

    def test_ninja_inputs(self) -> None:
        (inputs, _) = self._test([], [], [])
        self.assertEqual(
            {
                Path(self.build_dir / "tests_from_metadata.json"),
                Path(
                    self.build_dir
                    / "obj"
                    / "tests"
                    / "product_bundle_test_groups.json"
                ),
                Path(
                    self.build_dir
                    / "obj"
                    / "tests"
                    / "all_test_environments.json"
                ),
                Path(
                    self.build_dir
                    / "obj"
                    / "tests"
                    / "default_test_environments.json"
                ),
            },
            inputs,
        )

    def test_only_write_if_changed(self) -> None:
        tests_from_metadata = [
            {
                "test": {"name": "test1"},
                "environments": [{"dimensions": {"device_type": "AEMU"}}],
            },
            {
                "test": {"name": "test2"},
                "environments": [{"dimensions": {"device_type": "AEMU"}}],
            },
        ]
        tests_json = json.dumps(tests_from_metadata)
        tests_json_path = self.build_dir / "tests.json"
        tests_json_path.write_text(tests_json)
        previous_write_time = os.path.getmtime(tests_json_path)

        self._test(tests_from_metadata, [], [])

        # ensure the file did not change
        current_write_time = os.path.getmtime(tests_json_path)
        self.assertEqual(previous_write_time, current_write_time)

    def test_dimensions_dataclass(self) -> None:
        d1 = instance_from_dict(
            build_tests_json.Dimensions,
            {
                "os": "Linux",
                "cpu": "x64",
                "device_type": "QEMU",
                "host_device_type": "GCP_C4A_HIGHMEM_96_BM",
                "pool": "fuchsia.tests",
                "testbed": "my_testbed",
                "access_points": "1",
            },
        )
        d2 = build_tests_json.Dimensions(
            device_type="QEMU",
            cpu="x64",
            os="Linux",
            pool="fuchsia.tests",
            testbed="my_testbed",
            host_device_type="GCP_C4A_HIGHMEM_96_BM",
            access_points="1",
        )
        self.assertEqual(d1, d2)
        self.assertEqual(hash(d1), hash(d2))
        self.assertEqual(len({d1, d2}), 1)
        self.assertEqual(d1.cpu, "x64")
        self.assertEqual(d1.device_type, "QEMU")
        self.assertEqual(d1.host_device_type, "GCP_C4A_HIGHMEM_96_BM")
        self.assertEqual(d1.os, "Linux")
        self.assertEqual(d1.pool, "fuchsia.tests")
        self.assertEqual(d1.testbed, "my_testbed")
        self.assertEqual(d1.access_points, "1")
        self.assertEqual(d1.get("access_points"), "1")
        self.assertIsNone(d1.get("missing"))

        sub = build_tests_json.Dimensions(device_type="QEMU")
        self.assertTrue(sub.is_subset_of(d1))
        self.assertFalse(d1.is_subset_of(sub))

        with self.assertRaisesRegex(ValueError, "tags are only valid"):
            instance_from_dict(
                build_tests_json.Dimensions,
                {"device_type": "QEMU", "tags": ["foo"]},
            )
        with self.assertRaisesRegex(
            ValueError, "found nested dimensions environment field"
        ):
            instance_from_dict(
                build_tests_json.Dimensions,
                {"dimensions": {"device_type": "QEMU"}},
            )

    def test_emulator_config_dataclass(self) -> None:
        emu = instance_from_dict(
            build_tests_json.EmulatorConfig,
            {
                "name": "1cpu",
                "device": "x64-emu-min",
                "accel": "hyper",
                "kernel_args": ["arg1", "arg2"],
                "uefi": True,
                "vbmeta_key": "path/to/key.pem",
                "vbmeta_key_metadata": "path/to/meta.bin",
            },
        )
        self.assertEqual(
            instance_to_dict(emu),
            {
                "name": "1cpu",
                "device": "x64-emu-min",
                "accel": "hyper",
                "kernel_args": ["arg1", "arg2"],
                "uefi": True,
                "vbmeta_key": "path/to/key.pem",
                "vbmeta_key_metadata": "path/to/meta.bin",
            },
        )

        with self.assertRaisesRegex(ValueError, "requires a unique `name`"):
            instance_from_dict(
                build_tests_json.EmulatorConfig, {"device": "x64-emu-min"}
            )

        with self.assertRaisesRegex(
            ValueError, "must provide a `vbmeta_key` and `vbmeta_key_metadata`"
        ):
            instance_from_dict(
                build_tests_json.EmulatorConfig,
                {"name": "uefi_emu", "uefi": True},
            )

    def test_environment_dataclass_and_deduplication(self) -> None:
        env_a1 = instance_from_dict(
            build_tests_json.Environment,
            {
                "dimensions": {"device_type": "QEMU", "cpu": "x64"},
                "tags": ["tag1"],
            },
        )
        env_a2 = instance_from_dict(
            build_tests_json.Environment,
            {
                "dimensions": {"cpu": "x64", "device_type": "QEMU"},
                "tags": ["tag1"],
            },
        )
        env_b = instance_from_dict(
            build_tests_json.Environment,
            {"dimensions": {"device_type": "AEMU"}},
        )
        self.assertEqual(env_a1, env_a2)
        self.assertEqual(hash(env_a1), hash(env_a2))
        self.assertEqual(
            sorted({env_a1, env_b, env_a2}),
            [env_a1, env_b],
        )

        with self.assertRaisesRegex(
            ValueError, "each environment must specify dimensions"
        ):
            instance_from_dict(build_tests_json.Environment, {})
        with self.assertRaisesRegex(
            ValueError, "each environment must specify dimensions"
        ):
            instance_from_dict(build_tests_json.Environment, {"dimensions": {}})

    def test_partition_platforms(self) -> None:
        platforms = [
            {"device_type": "AEMU", "cpu": "x64"},
            {"device_type": "Vim3", "cpu": "arm64"},
            {"device_type": "Astro", "pool": "fuchsia.tests.connectivity"},
        ]
        target_p, other_p = build_tests_json.partition_platforms(
            platforms, "x64"
        )
        self.assertEqual(
            target_p,
            {
                instance_from_dict(
                    build_tests_json.Dimensions,
                    {"device_type": "AEMU", "cpu": "x64"},
                ),
                instance_from_dict(
                    build_tests_json.Dimensions,
                    {
                        "device_type": "Astro",
                        "pool": "fuchsia.tests.connectivity",
                    },
                ),
            },
        )
        self.assertEqual(
            other_p,
            {
                instance_from_dict(
                    build_tests_json.Dimensions,
                    {"device_type": "Vim3", "cpu": "arm64"},
                ),
            },
        )

    def test_unknown_platform_raises_value_error(self) -> None:
        tests_from_metadata = [
            {
                "test": {"name": "bad_env_test"},
                "environments": [
                    {"dimensions": {"device_type": "NonExistent"}}
                ],
            }
        ]
        with self.assertRaisesRegex(
            ValueError, "Could not match environment specifications"
        ):
            self._test(tests_from_metadata, [], [])

    def test_multiple_validation_errors_collected(self) -> None:
        tests_from_metadata = [
            {
                "test": {"name": "bad_platform_test", "label": "//src:test1"},
                "environments": [
                    {"dimensions": {"device_type": "NonExistent"}}
                ],
            },
            {
                "test": {"name": "bad_tags_test", "label": "//src:test2"},
                "environments": [
                    {"dimensions": {"device_type": "AEMU", "tags": ["oops"]}}
                ],
            },
        ]
        with self.assertRaises(ValueError) as ctx:
            self._test(tests_from_metadata, [], [])
        err_msg = str(ctx.exception)
        self.assertIn("bad_platform_test (//src:test1)", err_msg)
        self.assertIn("Could not match environment specifications", err_msg)
        self.assertIn("bad_tags_test (//src:test2)", err_msg)
        self.assertIn("tags are only valid in an environments scope", err_msg)

    def test_metadata_host_test_default_and_filtering(self) -> None:
        tests_from_metadata = [
            {
                "test": {"name": "pure_host_test", "os": "linux"},
                "expects_ssh": False,
            },
            {
                "test": {"name": "host_driven_target_test", "os": "linux"},
                "expects_ssh": True,
            },
            {
                "test": {"name": "filtered_out_test"},
                "environments": [{"dimensions": {"device_type": "Vim3"}}],
            },
            {
                "test": {"name": "filtered_out_host_device_type_test"},
                "environments": [
                    {
                        "dimensions": {
                            "device_type": "QEMU",
                            "host_device_type": "AmpereAltraMax-M128-30",
                        }
                    }
                ],
            },
            {
                "test": {"name": "explicit_build_only_test"},
                "build_only": True,
            },
        ]
        (_, tests) = self._test(tests_from_metadata, [], [])
        expected_tests = [
            {
                "test": {"name": "pure_host_test", "os": "linux"},
                "expects_ssh": False,
                "environments": [{"dimensions": {"cpu": "x64", "os": "Linux"}}],
            },
            {
                "test": {"name": "host_driven_target_test", "os": "linux"},
                "expects_ssh": True,
                "environments": [{"dimensions": {"device_type": "AEMU"}}],
            },
            {
                "test": {"name": "filtered_out_test"},
                "build_only": True,
                "environments": [],
            },
            {
                "test": {"name": "filtered_out_host_device_type_test"},
                "build_only": True,
                "environments": [],
            },
            {
                "test": {"name": "explicit_build_only_test"},
                "build_only": True,
                "environments": [],
            },
        ]
        self.assertEqual(expected_tests, tests)

    def test_product_bundle_test_group_environment_override(self) -> None:
        vim3_env = {"dimensions": {"device_type": "Vim3"}}
        astro_env = {"dimensions": {"device_type": "Astro"}}
        aemu_env = {"dimensions": {"device_type": "AEMU"}}

        pb_tests = [
            # No environments set: should inherit group's environments (NOT basic_envs).
            {"test": {"name": "default_pb_test"}},
            # Overlapping environments: should be overridden by group's environments.
            {
                "test": {"name": "subset_pb_test"},
                "environments": [vim3_env, aemu_env],
            },
            # Disjoint environments: should be overridden by group's environments.
            {
                "test": {"name": "disjoint_pb_test"},
                "environments": [aemu_env],
            },
            # Explicit build_only: remains build_only = True with empty environments.
            {
                "test": {"name": "build_only_pb_test"},
                "build_only": True,
            },
        ]
        tests_json_path = self.build_dir / "pb_tests.json"
        tests_json_path.write_text(json.dumps(pb_tests))

        test_groups = [
            {
                "product_bundle_name": "my_pb",
                "environments": [vim3_env, astro_env],
                "tests_json": str(tests_json_path),
            }
        ]
        product_bundles = [{"name": "my_pb"}]
        (_, tests) = self._test([], test_groups, product_bundles)

        expected_tests = [
            {
                "test": {"name": "default_pb_test-my_pb"},
                "product_bundle": "my_pb",
                "environments": [astro_env, vim3_env],
            },
            {
                "test": {"name": "subset_pb_test-my_pb"},
                "product_bundle": "my_pb",
                "environments": [astro_env, vim3_env],
            },
            {
                "test": {"name": "disjoint_pb_test-my_pb"},
                "product_bundle": "my_pb",
                "environments": [astro_env, vim3_env],
            },
            {
                "test": {"name": "build_only_pb_test-my_pb"},
                "product_bundle": "my_pb",
                "build_only": True,
                "environments": [],
            },
        ]
        self.assertEqual(expected_tests, tests)

    def test_product_bundle_test_group_no_environment_override(self) -> None:
        vim3_env = {"dimensions": {"device_type": "Vim3"}}
        astro_env = {"dimensions": {"device_type": "Astro"}}
        aemu_env = {"dimensions": {"device_type": "AEMU"}}

        pb_tests = [
            # No environments set: should inherit group's environments.
            {"test": {"name": "default_pb_test"}},
            # Overlapping environments: should be intersected with group's environments.
            {
                "test": {"name": "subset_pb_test"},
                "environments": [vim3_env, aemu_env],
            },
            # Disjoint environments: becomes build_only = True with empty environments.
            {
                "test": {"name": "disjoint_pb_test"},
                "environments": [aemu_env],
            },
        ]
        tests_json_path = self.build_dir / "pb_tests.json"
        tests_json_path.write_text(json.dumps(pb_tests))

        test_groups = [
            {
                "product_bundle_name": "my_pb",
                "environments": [vim3_env, astro_env],
                "override_test_environments": False,
                "tests_json": str(tests_json_path),
            }
        ]
        product_bundles = [{"name": "my_pb"}]
        (_, tests) = self._test([], test_groups, product_bundles)

        expected_tests = [
            {
                "test": {"name": "default_pb_test-my_pb"},
                "product_bundle": "my_pb",
                "environments": [astro_env, vim3_env],
            },
            {
                "test": {"name": "subset_pb_test-my_pb"},
                "product_bundle": "my_pb",
                "environments": [vim3_env],
            },
            {
                "test": {"name": "disjoint_pb_test-my_pb"},
                "product_bundle": "my_pb",
                "build_only": True,
                "environments": [],
            },
        ]
        self.assertEqual(expected_tests, tests)

    def test_cross_arch_linux_host_test(self) -> None:
        tests_from_metadata = [
            {
                "test": {"name": "x64_host_test", "os": "linux", "cpu": "x64"},
                "expects_ssh": False,
            },
            {
                "test": {
                    "name": "arm64_host_test",
                    "os": "linux",
                    "cpu": "arm64",
                },
                "expects_ssh": False,
            },
        ]
        default_test_environments = {
            "target_cpu": "arm64",
            "default_environments": [
                {
                    "dimensions": {
                        "device_type": "QEMU",
                        "host_device_type": "GCP_C4A_HIGHMEM_96_BM",
                    }
                }
            ],
            "allowed_device_types": ["QEMU"],
            "allowed_host_device_types": ["GCP_C4A_HIGHMEM_96_BM"],
        }
        (_, tests) = self._test(
            tests_from_metadata,
            [],
            [],
            default_test_environments=default_test_environments,
        )
        expected_tests = [
            {
                "test": {"name": "x64_host_test", "os": "linux", "cpu": "x64"},
                "expects_ssh": False,
                "build_only": True,
                "environments": [],
            },
            {
                "test": {
                    "name": "arm64_host_test",
                    "os": "linux",
                    "cpu": "arm64",
                },
                "expects_ssh": False,
                "environments": [
                    {"dimensions": {"cpu": "arm64", "os": "Linux"}}
                ],
            },
        ]
        self.assertEqual(expected_tests, tests)

    def test_resolve_test_environments_helper(self) -> None:
        target_p, other_p = build_tests_json.partition_platforms(
            [
                {"device_type": "AEMU", "cpu": "x64"},
                {"device_type": "Vim3", "cpu": "arm64"},
            ],
            "x64",
        )
        aemu_env = build_tests_json.Environment(
            dimensions=build_tests_json.Dimensions(device_type="AEMU")
        )
        vim3_env = build_tests_json.Environment(
            dimensions=build_tests_json.Dimensions(device_type="Vim3")
        )

        # Basic test mode (restrict_to_default_envs=False)
        test_basic: dict[str, T.Any] = {"test": {"name": "t1"}}
        build_tests_json.resolve_test_environments(
            test_basic,
            default_envs=[aemu_env],
            allowed_device_types={"AEMU"},
            allowed_host_device_types=set(),
            target_platforms=target_p,
            other_platforms=other_p,
            restrict_to_default_envs=False,
        )
        self.assertEqual(
            test_basic["environments"],
            [{"dimensions": {"device_type": "AEMU"}}],
        )

        # Restricted mode (restrict_to_default_envs=True, e.g. product_bundle_test_group)
        test_pb: dict[str, T.Any] = {
            "test": {"name": "t2"},
            "environments": [
                {"dimensions": {"device_type": "AEMU"}},
                {"dimensions": {"device_type": "Vim3"}},
            ],
        }
        build_tests_json.resolve_test_environments(
            test_pb,
            default_envs=[vim3_env],
            allowed_device_types={"Vim3"},
            allowed_host_device_types=set(),
            target_platforms=target_p,
            other_platforms=other_p,
            restrict_to_default_envs=True,
        )
        self.assertEqual(
            test_pb["environments"], [{"dimensions": {"device_type": "Vim3"}}]
        )

        # Invalid test environment raises ValueError immediately
        test_invalid: dict[str, T.Any] = {
            "test": {"name": "bad"},
            "environments": [{"dimensions": {"device_type": "UnknownDevice"}}],
        }
        with self.assertRaises(ValueError):
            build_tests_json.resolve_test_environments(
                test_invalid,
                default_envs=[aemu_env],
                allowed_device_types={"AEMU"},
                allowed_host_device_types=set(),
                target_platforms=target_p,
                other_platforms=other_p,
                restrict_to_default_envs=False,
            )


if __name__ == "__main__":
    unittest.main()
