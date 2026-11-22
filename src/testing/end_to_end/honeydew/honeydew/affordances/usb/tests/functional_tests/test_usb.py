# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Mobly test for USB affordance."""

import fuchsia_base_test
from honeydew import errors
from mobly import asserts, test_runner


class UsbAffordanceTests(fuchsia_base_test.FuchsiaBaseTest):
    """USB affordance functional tests."""

    def test_get_diagnostic_info(self) -> None:
        """Test case for USB.get_diagnostic_info()."""
        try:
            output: str = self.dut.usb.get_diagnostic_info()
        except errors.NotSupportedError as err:
            asserts.skip(str(err))
        asserts.assert_true(
            bool(output), "Expected non-empty USB diagnostic info over serial."
        )

    def test_usb_disconnect_and_connect(self) -> None:
        """Test case for USB.disconnect(), USB.connect(), and callback registration."""
        events: list[str] = []
        self.dut.usb.register_for_before_disconnect(
            lambda: events.append("before_disconnect")
        )
        self.dut.usb.register_for_after_connect(
            lambda: events.append("after_connect")
        )

        try:
            self.dut.usb.disconnect()
            asserts.assert_equal(events, ["before_disconnect"])
        finally:
            self.dut.usb.connect()
        asserts.assert_equal(events, ["before_disconnect", "after_connect"])


if __name__ == "__main__":
    test_runner.main()
