# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""A trivial Mobly test, used to test the python_mobly_test() Bazel macro."""

from mobly import base_test, test_runner


class FakeMoblyTest(base_test.BaseTestClass):
    """A Mobly test that does not need any device."""

    def test_pass(self) -> None:
        """A test case that always passes."""


if __name__ == "__main__":
    test_runner.main()
