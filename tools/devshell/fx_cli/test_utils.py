# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Shared helpers for fx_cli unit tests."""

import pathlib
import sys
from collections.abc import Mapping


def isolated_python_env(
    extra_env: Mapping[str, str],
    *,
    executable: str = sys.executable,
    version_info: tuple[int, int] = (
        sys.version_info.major,
        sys.version_info.minor,
    ),
) -> dict[str, str]:
    """Build a minimal env for running sys.executable without workspace PYTHONPATH.

    Under Bazel host_py_test, sys.executable points to the compact
    @prebuilt_python interpreter (`python<X>.<Y>-real`) whose stdlib lives in a
    sibling `lib_python<X>.<Y>.zip`. Pass only PYTHONHOME and that stdlib zip so
    CPython can start on isolated Swarming shards without exposing workspace or
    runfiles paths on sys.path.
    """
    env = dict(extra_env)
    # Do not call .resolve(): python<X>.<Y>-real is a symlink pointing outside
    # the compact runtime directory.
    py_path = pathlib.Path(executable).absolute()
    stdlib_zip = (
        py_path.parent / f"lib_python{version_info[0]}.{version_info[1]}.zip"
    )
    if py_path.name.endswith("-real") and stdlib_zip.is_file():
        env["PYTHONHOME"] = str(py_path.parent)
        env["PYTHONPATH"] = str(stdlib_zip)
    return env
