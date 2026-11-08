# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Mobly test for AndroidIdle affordance."""

import time
from datetime import timedelta

import fuchsia_base_test
from honeydew.affordances.android_idle import android_idle
from honeydew.device_classes import android_device
from mobly import asserts, test_runner

_AFTER_BOOT_DURATION: timedelta = timedelta(seconds=5)
_DEFAULT_IDLED_AFTER_BOOT_PROP: str = "debug.honeydew.idled_after_boot"


class AndroidIdleAffordanceTests(fuchsia_base_test.FuchsiaBaseTest):
    """AndroidIdle affordance tests."""

    dut: android_device.AndroidDevice

    async def setup_class(self) -> None:
        """setup_class is called once before running tests."""
        await super().setup_class()
        asserts.assert_is_instance(self.dut, android_device.AndroidDevice)
        self._orig_idled_after_boot_prop: str = self.dut.idled_after_boot_prop
        self._orig_after_boot_duration: timedelta | None = (
            self.dut.after_boot_duration
        )
        if not self.dut.idled_after_boot_prop:
            self.dut.idled_after_boot_prop = _DEFAULT_IDLED_AFTER_BOOT_PROP
        self.dut.after_boot_duration = _AFTER_BOOT_DURATION
        self.android_idle: android_idle.AndroidIdle = self.dut.android_idle
        asserts.assert_is_instance(self.android_idle, android_idle.AndroidIdle)

    async def teardown_class(self) -> None:
        """teardown_class is called once after running tests."""
        try:
            if self.dut.idled_after_boot_prop:
                self.dut.adb.setprop(self.dut.idled_after_boot_prop, "")
        finally:
            self.dut.idled_after_boot_prop = self._orig_idled_after_boot_prop
            self.dut.after_boot_duration = self._orig_after_boot_duration
            await super().teardown_class()

    def test_verify_supported(self) -> None:
        """Test case for AndroidIdle.verify_supported()."""
        self.android_idle.verify_supported()

    async def test_idle(self) -> None:
        """Test case for AndroidIdle.idle()."""
        idle_duration = timedelta(seconds=1)
        start: float = time.monotonic()
        await self.android_idle.idle(idle_duration, msg="functional test idle")
        asserts.assert_greater_equal(
            time.monotonic() - start, idle_duration.total_seconds() - 0.05
        )

    async def test_idle_after_boot(self) -> None:
        """Test case for AndroidIdle.idle_after_boot() on-device property state transitions."""
        asserts.assert_true(
            bool(self.dut.idled_after_boot_prop),
            "idled_after_boot_prop must be configured for this test",
        )
        self.dut.adb.setprop(self.dut.idled_after_boot_prop, "")
        asserts.assert_equal(
            self.dut.adb.getprop(self.dut.idled_after_boot_prop).strip(), ""
        )

        first_start: float = time.monotonic()
        await self.android_idle.idle_after_boot()
        asserts.assert_greater_equal(
            time.monotonic() - first_start,
            _AFTER_BOOT_DURATION.total_seconds() - 0.05,
        )
        prop: str = self.dut.adb.getprop(self.dut.idled_after_boot_prop).strip()
        asserts.assert_true(
            prop.isdigit(), f"Idle timestamp was not recorded: {prop!r}"
        )

        # At least _AFTER_BOOT_DURATION has passed since the recorded
        # timestamp, so a second call must skip idling and preserve the property.
        start: float = time.monotonic()
        await self.android_idle.idle_after_boot(duration=_AFTER_BOOT_DURATION)
        asserts.assert_less(
            time.monotonic() - start, _AFTER_BOOT_DURATION.total_seconds()
        )
        asserts.assert_equal(
            self.dut.adb.getprop(self.dut.idled_after_boot_prop).strip(), prop
        )


if __name__ == "__main__":
    test_runner.main()
