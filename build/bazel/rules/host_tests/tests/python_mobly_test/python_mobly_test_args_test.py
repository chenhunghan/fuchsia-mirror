#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Checks the arguments that python_mobly_test() targets pass to mobly_test_parser.

The arguments are read from the `exec` line of each target's generated host
test launcher script.
"""

import os
import shlex
import unittest

import python.runfiles.runfiles as runfiles

_PACKAGE = "build/bazel/rules/host_tests/tests/python_mobly_test"


def _driver_and_test_args(test_name: str) -> list[str]:
    return [
        "mobly_test_parser.runfiles/_main/src/testing/end_to_end/mobly_driver/mobly_driver_binary_target",
        f"mobly_test_parser.runfiles/_main/{_PACKAGE}/{test_name}_bin",
        "--hermetic",
        "--ffx-path",
        "host-tools/ffx",
        "--ffx-subtools-path",
        "host-tools",
        "--ssh-path",
        f"{test_name}/ssh",
    ]


# The python_mobly_test() targets, which must be listed in the test's
# `test_data`, and their expected arguments after the common ones.
_CASES = {
    "fake_mobly_test_default": [],
    "fake_mobly_test_all_options": [
        "--params-yaml-path",
        "fake_mobly_test_all_options/test_params.yaml",
        "--honeydew-config-json-path",
        "fake_mobly_test_all_options/honeydew_config.json",
        "--config-yaml-path",
        "fake_mobly_test_all_options/config.yaml",
        "--ssh-key",
        "fake_mobly_test_all_options/ssh_key",
        "--test-timeout-sec",
        "600",
        "--cleanup-period-sec",
        "30",
        "--multi-device",
        "--target-address-type",
        "usb",
        "--v",
        "--test_cases",
        "test_pass",
    ],
    "fake_mobly_test_params_source": [
        "--params-yaml-path",
        "fake_mobly_test_params_source/test_params.yaml",
    ],
    "fake_mobly_test_custom": [
        "--config-yaml-path",
        "fake_mobly_test_custom/config.yaml",
    ],
}

# Runtime files that python_mobly_test() targets must have, relative to their
# runtime directory, e.g. from `srcs`, `libraries`, `data` and
# `test_data_deps`.
_RUNFILES_DIR = "mobly_test_parser.runfiles/_main"
_RUNTIME_FILES = {
    "fake_mobly_test_custom": [
        f"{_RUNFILES_DIR}/{_PACKAGE}/fake_mobly_test_custom.py",
        f"{_RUNFILES_DIR}/{_PACKAGE}/fake_mobly_test_custom_main_callable.py",
        f"{_RUNFILES_DIR}/{_PACKAGE}/fake_mobly_test_helper.py",
        f"{_RUNFILES_DIR}/{_PACKAGE}/testdata/extra_test_data.txt",
        f"{_RUNFILES_DIR}/{_PACKAGE}/testdata/test_data.txt",
        "fake_mobly_test_custom/config.yaml",
        "fake_mobly_test_custom/ssh",
        "host-tools/ffx",
        "host-tools/ffx-component",
    ],
}


def _launcher_exec_args(launcher: str) -> list[str]:
    """Returns the arguments of the `exec` line of a host test launcher."""
    with open(launcher) as f:
        exec_lines = [line for line in f if line.startswith("exec ")]
    assert len(exec_lines) == 1, f"Expected one exec line in {launcher}"
    return shlex.split(exec_lines[0])


class PythonMoblyTestArgsTest(unittest.TestCase):
    def test_runtime_files(self) -> None:
        r = runfiles.Create()
        assert r is not None, "Cannot find runfiles"
        for test_name, runtime_files in _RUNTIME_FILES.items():
            rlocation = f"_main/{_PACKAGE}/{test_name}.runtime_dir"
            runtime_dir = r.Rlocation(rlocation)
            assert runtime_dir is not None and os.path.isdir(
                runtime_dir
            ), f"Cannot find {rlocation} in runfiles"
            for runtime_file in runtime_files:
                with self.subTest(test_name=test_name, file=runtime_file):
                    self.assertTrue(
                        os.path.exists(os.path.join(runtime_dir, runtime_file)),
                        f"Missing {runtime_file} in {runtime_dir}",
                    )

    def test_args(self) -> None:
        r = runfiles.Create()
        assert r is not None, "Cannot find runfiles"
        for test_name, extra_args in _CASES.items():
            with self.subTest(test_name=test_name):
                rlocation = f"_main/{_PACKAGE}/{test_name}"
                launcher = r.Rlocation(rlocation)
                assert launcher is not None and os.path.exists(
                    launcher
                ), f"Cannot find {rlocation} in runfiles"
                self.assertEqual(
                    _launcher_exec_args(launcher),
                    [
                        "exec",
                        "env",
                        "PYTHONUNBUFFERED=1",
                        "./mobly_test_parser",
                    ]
                    + _driver_and_test_args(test_name)
                    + extra_args
                    + ["$@"],
                )
