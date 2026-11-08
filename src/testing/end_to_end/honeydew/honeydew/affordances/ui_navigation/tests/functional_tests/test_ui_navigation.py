# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Mobly test for UI navigation affordance."""

from typing import Any

import fuchsia_base_test
from honeydew.affordances.ui.screenshot.types import ScreenshotImage
from honeydew.affordances.ui.user_input import types as ui_types
from honeydew.affordances.ui_navigation import ui_navigation
from honeydew.device_classes import android_device
from mobly import asserts, test_runner


class UiNavigationAffordanceTests(fuchsia_base_test.FuchsiaBaseTest):
    """UI navigation affordance functional tests."""

    dut: android_device.AndroidDevice

    async def setup_class(self) -> None:
        """setup_class is called once before running tests."""
        await super().setup_class()
        asserts.assert_is_instance(self.dut, android_device.AndroidDevice)

    async def teardown_class(self) -> None:
        """teardown_class is called once after running all tests."""
        self.dut.ui_navigation.close()
        await super().teardown_class()

    async def test_ui_navigation_affordance_and_dialogs(self) -> None:
        """Verifies ui_navigation affordance, screenshot check, and dialog dismissal."""
        nav = self.dut.ui_navigation
        asserts.assert_is_instance(nav, ui_navigation.UiNavigation)
        nav.verify_supported()
        asserts.assert_is_not_none(nav.adb)

        empty_img = ScreenshotImage(size=ui_types.Size(0, 0), data=b"")
        asserts.assert_true(
            nav.is_black_screenshot(empty_img),
            "Expected empty screenshot to be classified as black.",
        )

        if hasattr(self.dut, "screenshot") and self.dut.screenshot is not None:
            live_img = self.dut.screenshot.take()
            asserts.assert_false(
                nav.is_black_screenshot(live_img),
                "Expected live screenshot to not be classified as black.",
            )

        dismissed_adb = nav.dismiss_adb_auth_dialog(
            self.dut.adb_auth_activity, self.dut.sysui_package
        )
        asserts.assert_is_instance(dismissed_adb, bool)

        dismissed_nav = nav.dismiss_blocking_dialogs()
        asserts.assert_is_instance(dismissed_nav, bool)

        dismissed = self.dut.dismiss_blocking_dialogs()
        asserts.assert_is_instance(dismissed, bool)

        await nav.check_and_mitigate_black_screen()

    async def test_return_to_home_and_idle_screen(self) -> None:
        """Verifies return_to_home, wait_for_idle_screen, verify_is_idle_screen, and wait_for_launcher_screen."""
        self.dut.ui_navigation.return_to_home()
        self.dut.ui_navigation.return_to_idle_screen()
        await self.dut.ui_navigation.wait_for_idle_screen()
        await self.dut.ui_navigation.verify_is_idle_screen()
        await self.dut.ui_navigation.wait_for_launcher_screen()

    async def test_uiautomator_device_and_task(self) -> None:
        """Verifies create_uiautomator_device, run_uiautomator_task, and is_idle_screen."""
        ui_dev = self.dut.ui_navigation.create_uiautomator_device()
        asserts.assert_is_not_none(ui_dev)

        def _check_idle(dev: Any) -> bool:
            return self.dut.ui_navigation.is_idle_screen(dev.ui)

        result = await self.dut.ui_navigation.run_uiautomator_task(_check_idle)
        asserts.assert_is_instance(result, bool)

    async def test_close_and_reinitialize(self) -> None:
        """Verifies close() releases resources and subsequent calls re-initialize."""
        nav = self.dut.ui_navigation
        ui_dev = nav.create_uiautomator_device()
        asserts.assert_is_not_none(ui_dev)
        nav.close()
        nav.verify_supported()
        reopened_dev = nav.create_uiautomator_device()
        asserts.assert_is_not_none(reopened_dev)


if __name__ == "__main__":
    test_runner.main()
