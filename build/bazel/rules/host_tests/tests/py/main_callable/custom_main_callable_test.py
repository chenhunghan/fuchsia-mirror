# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""A host_py_test() main whose custom main_callable receives the test arguments."""

import sys


def main() -> int:
    if sys.argv[1:] != ["--expected-arg"]:
        print(f"ERROR: Unexpected arguments: {sys.argv[1:]}", file=sys.stderr)
        return 1
    print("custom main called")
    return 0


if __name__ == "__main__":
    # The generated entry point imports this module, so this must never run.
    print("ERROR: __main__ block should not run", file=sys.stderr)
    sys.exit(1)
