#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Entry point of a `py_zipapp_binary()` exercising `py_data_package()`."""

import importlib.resources
import sys

import dotted_pkg.nested
import pkg_with_data.reader as reader
import test_data


def main() -> int:
    greeting = reader.read_greeting()
    untyped = reader.read_untyped_py_data()
    tool_out = reader.run_echo_tool("from_zip")
    isolated = (
        importlib.resources.files(test_data)
        .joinpath("extra.txt")
        .read_text()
        .strip()
    )
    nested = (
        importlib.resources.files(dotted_pkg.nested)
        .joinpath("custom.txt")
        .read_text()
        .strip()
    )
    print(f"{greeting}|{untyped}|{tool_out}|{isolated}|{nested}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
