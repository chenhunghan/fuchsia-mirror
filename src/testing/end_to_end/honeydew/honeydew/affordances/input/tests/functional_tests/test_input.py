# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Mobly test for input (Gestures, Buttons) affordances."""

import fuchsia_base_test
from honeydew.affordances.input import input as input_affordance
from honeydew.device_classes import android_device
from mobly import asserts, test_runner


class InputAffordanceTests(fuchsia_base_test.FuchsiaBaseTest):
    """Input affordance functional tests."""

    dut: android_device.AndroidDevice

    async def setup_class(self) -> None:
        """setup_class is called once before running tests."""
        await super().setup_class()
        asserts.assert_is_instance(self.dut, android_device.AndroidDevice)

    async def test_gestures_touch_and_swipes(self) -> None:
        """Verifies Gestures touch coordinate calculation, taps, and swipes."""
        asserts.assert_is_instance(self.dut.gestures, input_affordance.Gestures)
        self.dut.gestures.verify_supported()
        await self.dut.gestures.make_ready()

        center = self.dut.gestures.screen_coordinate(0.5, 0.5)
        asserts.assert_greater(center.x, 0)
        asserts.assert_greater(center.y, 0)

        await self.dut.gestures.tap(center.x, center.y)
        await self.dut.gestures.tap_bottom()
        await self.dut.gestures.swipe_left()
        await self.dut.gestures.swipe_right()
        await self.dut.gestures.swipe_up()
        await self.dut.gestures.swipe_down()
        await self.dut.gestures.drag(0.2, 0.5, 0.8, 0.5)
        dumpsys_out = self.dut.adb.run(["shell", "dumpsys", "input"])
        asserts.assert_in("InputDispatcher", dumpsys_out)

    def test_buttons_volume_and_power(self) -> None:
        """Verifies Buttons volume and power keyevents on device."""
        asserts.assert_is_instance(self.dut.buttons, input_affordance.Buttons)
        self.dut.buttons.verify_supported()

        self.dut.buttons.volume_up()
        self.dut.buttons.volume_down()
        self.dut.buttons.volume_mute()
        self.dut.buttons.power()
        self.dut.buttons.power()
        dumpsys_out = self.dut.adb.run(["shell", "dumpsys", "input"])
        asserts.assert_in("InputDispatcher", dumpsys_out)

    async def test_buttons_navigation(self) -> None:
        """Verifies Buttons home keyevent and rotary crown scrolling."""
        asserts.assert_is_instance(self.dut.buttons, input_affordance.Buttons)
        self.dut.buttons.verify_supported()

        self.dut.buttons.home()
        await self.dut.buttons.crown_scroll(scroll_v_detent=1)
        await self.dut.buttons.crown_scroll(delta=-1)
        self.dut.buttons.home()
        dumpsys_out = self.dut.adb.run(["shell", "dumpsys", "input"])
        asserts.assert_in("InputDispatcher", dumpsys_out)


if __name__ == "__main__":
    test_runner.main()
