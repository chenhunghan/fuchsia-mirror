# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Mobly test for HelloFuchsia affordance."""

import logging

import fuchsia_base_test
from mobly import asserts, test_runner

LOGGER: logging.Logger = logging.getLogger(__name__)


class HelloFuchsiaAffordanceTests(fuchsia_base_test.FuchsiaBaseTest):
    """HelloFuchsia affordance tests."""

    async def setup_class(self) -> None:
        await super().setup_class()

    async def test_hello_fuchsia_greeting(self) -> None:
        """Test case for HelloFuchsia.greeting() method"""
        asserts.assert_equal(
            await self.dut.hello_fuchsia.greeting(),
            f"Hello Fuchsia, {self.dut.device_name}!",
        )


if __name__ == "__main__":
    test_runner.main()
