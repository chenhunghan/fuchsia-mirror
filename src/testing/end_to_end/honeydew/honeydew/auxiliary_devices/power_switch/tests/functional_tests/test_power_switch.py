# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Mobly test for PowerSwitch interface."""

import logging

import fuchsia_base_test
from honeydew.auxiliary_devices.power_switch import power_switch
from mobly import asserts, test_runner

_LOGGER: logging.Logger = logging.getLogger(__name__)


class PowerSwitchTest(fuchsia_base_test.FuchsiaBaseTest):
    """Mobly test for PowerSwitch implementations."""

    _power_switch: power_switch.PowerSwitch
    _outlet_arg: int | None

    async def setup_class(self) -> None:
        """setup_class is called once before running tests."""
        await super().setup_class()

        power_switch_dev, outlet = self._lookup_power_switch(self.dut)
        if power_switch_dev is None:
            asserts.abort_class(
                "No PowerSwitch configured (neither DMC_PATH environment "
                "variable nor power_switch_hw/power_switch_impl in device "
                "config)."
            )
        assert power_switch_dev is not None
        self._power_switch = power_switch_dev
        self._outlet_arg = outlet

    async def test_power_switch(self) -> None:
        """Test case for PowerSwitch.power_off and PowerSwitch.power_on."""
        _LOGGER.info(
            "Testing power switch using %s",
            self._power_switch.__class__.__name__,
        )

        # Check if device is online before powering off
        await self.dut.wait_for_online()

        # power off the device using the dynamically determined outlet argument
        _LOGGER.info("Starting power_off test cycle.")
        self.dut.ffx.notify_intentional_disconnect()
        self._power_switch.power_off(outlet=self._outlet_arg)

        self.dut.wait_for_offline()

        # power on the device using the dynamically determined outlet argument
        _LOGGER.info("Starting power_on test cycle.")
        self._power_switch.power_on(outlet=self._outlet_arg)

        await self.dut.wait_for_online()
        await self.dut.on_device_boot()

        _LOGGER.info("Power cycle test completed successfully.")


if __name__ == "__main__":
    test_runner.main()
