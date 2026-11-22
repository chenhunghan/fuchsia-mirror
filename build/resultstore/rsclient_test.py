#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for build.resultstore.rsclient."""

import pathlib
import unittest

from build.resultstore import rsclient


class RsclientTest(unittest.TestCase):
    def test_bin_dir(self) -> None:
        fuchsia_dir = pathlib.Path("/path/to/fuchsia")
        self.assertEqual(
            rsclient.bin_dir(fuchsia_dir, "linux-x64"),
            fuchsia_dir / "prebuilt" / "rsclient" / "linux-x64" / "bin",
        )
        self.assertEqual(
            rsclient.bin_dir(fuchsia_dir, "mac-arm64"),
            fuchsia_dir / "prebuilt" / "rsclient" / "mac-arm64" / "bin",
        )


if __name__ == "__main__":
    unittest.main()
