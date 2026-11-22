# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Reads resources from a `py_data_package()` subpackage."""

import importlib.resources
import os
import stat
import subprocess

from pkg_with_data import data


def read_greeting() -> str:
    return (
        importlib.resources.files(data)
        .joinpath("greeting.txt")
        .read_text()
        .strip()
    )


def read_untyped_py_data() -> str:
    return (
        importlib.resources.files(data)
        .joinpath("gen_bad_data.py")
        .read_text()
        .strip()
    )


def run_echo_tool(arg: str, require_pre_executable: bool = False) -> str:
    with importlib.resources.as_file(
        importlib.resources.files(data).joinpath("renamed_echo_tool")
    ) as tool_path:
        if require_pre_executable and not os.access(tool_path, os.X_OK):
            raise PermissionError(f"{tool_path} is not executable")
        if not os.access(tool_path, os.X_OK):
            tool_path.chmod(tool_path.stat().st_mode | stat.S_IEXEC)
        return subprocess.check_output(
            (str(tool_path), arg),
            text=True,
        ).strip()
