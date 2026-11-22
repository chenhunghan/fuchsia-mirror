#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Main source of a py_zipapp_binary() test binary.

Exercises each way a module can be found in the archive, then prints a line
that the tests check, or writes it to the file given with `--output` (so that
the build stays quiet when a `python_action()` runs it).
"""

import argparse
import importlib.resources
import pathlib
import sys

import zipapp_test_data

# A source of the binary, at the archive root.
import zipapp_test_helper

# A library source, through its `imports` root.
import zipapp_test_lib

# Sources through the repository root, like in py_binary().
from build.bazel.rules.python.tests import (
    zipapp_test_helper as qualified_helper,
)
from build.bazel.rules.python.tests.lib import zipapp_test_lib as qualified_lib


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=pathlib.Path)
    args = parser.parse_args()

    if qualified_helper.NAME != zipapp_test_helper.NAME:
        print("Mismatched helper modules", file=sys.stderr)
        return 1
    if qualified_lib.message() != zipapp_test_lib.message():
        print("Mismatched library modules", file=sys.stderr)
        return 1
    greeting = (
        importlib.resources.files(zipapp_test_data)
        .joinpath("greeting.txt")
        .read_text()
    )
    line = (
        f"{zipapp_test_helper.NAME}: {zipapp_test_lib.message()}, "
        f"{greeting.strip()}"
    )
    if args.output is None:
        print(line)
    else:
        args.output.write_text(line + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
