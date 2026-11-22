#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Updates the image-format coverage goldens from the C++ implementation.

Runs image-format-test, which is linked against the C++ implementation of
image-format, with `fx test`. For each golden that doesn't match, the test
writes a replacement image-format-coverage-golden-<name>.inc file to
/custom_artifacts, which `fx test` copies to its output directory. This copies
those files over the goldens next to this script.

Usage: update-coverage-goldens.py [additional `fx test` arguments]
"""

import glob
import os
import shutil
import subprocess
import sys
import tempfile

TEST_DIR = os.path.dirname(os.path.abspath(__file__))
GOLDEN_PREFIX = "image-format-coverage-golden-"


def main() -> int:
    with tempfile.TemporaryDirectory() as outdir:
        # The test fails when a golden doesn't match, which is expected here, so
        # don't check the exit code upfront.
        result = subprocess.run(
            ["fx", "test", "image-format-test", "--outdir", outdir]
            + sys.argv[1:],
            check=False,
        )
        written = glob.glob(
            os.path.join(outdir, "**", GOLDEN_PREFIX + "*.inc"), recursive=True
        )
        if not written:
            if result.returncode != 0:
                print(
                    "fx test failed without writing any replacement goldens "
                    "(for example, from a build error or a non-golden test "
                    "failure; see the output above).",
                    file=sys.stderr,
                )
                return result.returncode
            print("All coverage goldens already match.")
            return 0
        for path in sorted(written):
            name = os.path.basename(path)
            shutil.copyfile(path, os.path.join(TEST_DIR, name))
            print(f"Updated {name}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
