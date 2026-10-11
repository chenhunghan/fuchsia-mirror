# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest

from fidl_api_compatibility_test import (
    GoldenMismatchError,
    golden_not_found_error,
)


class FidlApiCompatibilityTestTest(unittest.TestCase):
    def test_golden_mismatch_error_with_update_hint(self):
        err = GoldenMismatchError(
            api_level="NEXT",
            current="/path/to/current",
            golden="/path/to/golden",
            show_update_hint=True,
            show_fidl_availability_hint=False,
        )
        msg = str(err)
        self.assertIn("Detected changes to API level NEXT", msg)
        self.assertIn(
            "Please acknowledge this change by updating the golden.", msg
        )
        self.assertIn("update_goldens=true", msg)
        self.assertIn("fx args", msg)

    def test_golden_mismatch_error_without_update_hint(self):
        err = GoldenMismatchError(
            api_level="28",
            current="/path/to/current",
            golden="/path/to/golden",
            show_update_hint=False,
            show_fidl_availability_hint=False,
        )
        msg = str(err)
        self.assertIn("Detected changes to API level 28", msg)
        self.assertNotIn("update_goldens=true", msg)

    def test_golden_not_found_error(self):
        err = golden_not_found_error("/path/to/missing_golden")
        msg = str(err)
        self.assertIn(
            "The golden file /path/to/missing_golden does not exist.", msg
        )
        self.assertIn("touch /path/to/missing_golden", msg)
        self.assertIn("update_goldens=true", msg)
        self.assertIn("fx args", msg)


if __name__ == "__main__":
    unittest.main()
