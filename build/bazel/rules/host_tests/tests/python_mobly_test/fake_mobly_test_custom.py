# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""A Mobly test with a custom `main_callable` and a library, used to test python_mobly_test()."""

import fake_mobly_test_helper
from mobly import asserts, base_test, test_runner


class FakeCustomMoblyTest(base_test.BaseTestClass):
    """A Mobly test that does not need any device."""

    def test_helper(self) -> None:
        """Checks that the library from `libraries` can be used."""
        asserts.assert_equal(fake_mobly_test_helper.answer(), 42)


def custom_main() -> None:
    """The `main_callable` of this test."""
    test_runner.main()
