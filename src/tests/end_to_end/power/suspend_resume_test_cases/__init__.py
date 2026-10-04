# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import logging
from datetime import timedelta

import fuchsia_base_test
from honeydew.auxiliary_devices.usb_power_hub import usb_power_hub
from honeydew.fuchsia_device.fuchsia_device import FuchsiaDevice
from honeydew.transports.ffx import types as ffx_types
from honeydew.utils import control_flows, power
from honeydew.utils.deadline import Deadline
from mobly.asserts import assert_equal, assert_less

_LOGGER: logging.Logger = logging.getLogger(__name__)

# Timeout for automated suspend/resume runs (e.g. with DMC in infra).
_SUSPEND_RESUME_TIMEOUT: timedelta = timedelta(minutes=1)

# More forgiving timeouts for manual at-desk testing where a user physically
# unplugs and replugs the USB cable.
_MANUAL_SUSPEND_RESUME_TIMEOUT: timedelta = timedelta(minutes=5)
_MANUAL_BASE_IDLE_DURATION: timedelta = timedelta(seconds=10)


class _ManualUsbPowerHub(usb_power_hub.UsbPowerHub):
    """Prompts the user to manually unplug and replug the USB cable.

    Used during local at-desk testing when no infra-provided USB disconnector
    (such as DMC, a programmable USB power hub, or a virtual USB hub) is
    available.
    """

    def __init__(self, device_name: str) -> None:
        super().__init__()
        self._device_name = device_name

    def power_off(self, port: int | None = None) -> None:
        """Logs instructions for the user to unplug the USB cable."""
        del port
        _LOGGER.info(
            "[ACTION REQUIRED] Please UNPLUG the USB cable from %s now so the "
            "device can suspend (waiting for device to go offline)...",
            self._device_name,
        )

    def power_on(self, port: int | None = None) -> None:
        """Logs instructions for the user to replug the USB cable."""
        del port
        _LOGGER.info(
            "[ACTION REQUIRED] Please PLUG the USB cable back into %s now to "
            "resume the device (waiting for device to come back online)...",
            self._device_name,
        )


class SuspendResumeTestCases(fuchsia_base_test.FuchsiaTestCases):
    """Test cases for suspend and resume."""

    def _set_display_power(self, device: FuchsiaDevice, power_on: bool) -> None:
        """Power on/off the display panel.

        Args:
            device: Fuchsia device object.
            power_on: True to power on the display, False to power off.
        """
        state = "on" if power_on else "off"
        _LOGGER.info(
            "Setting display panel power to %s on %s...",
            state,
            device.device_name,
        )
        try:
            device.ffx.run_ssh_cmd(f"display-tweak panel --power {state}")
        except Exception as e:  # pylint: disable=broad-except
            _LOGGER.warning("Failed to power %s display panel: %s", state, e)

    def _set_battery_manager_running(
        self, device: FuchsiaDevice, running: bool
    ) -> None:
        """Start or stop `/core/battery_manager` around suspend.

        On workbench configurations with a battery, `/core/battery_manager`
        holds a `charging_block_suspension` wake lease that prevents SAG from
        entering suspend if charger/fuel-gauge disconnect updates are not yet
        propagated.
        """
        action = "start" if running else "stop"
        _LOGGER.info(
            "Running `ffx component %s /core/battery_manager` on %s...",
            action,
            device.device_name,
        )
        try:
            device.ffx.run(
                ["component", action, "/core/battery_manager"],
                machine=ffx_types.MachineFormat.RAW,
            )
        except Exception as e:  # pylint: disable=broad-except
            _LOGGER.warning("Failed to %s /core/battery_manager: %s", action, e)

    def _ensure_usb_power_hub(self, device: FuchsiaDevice) -> bool:
        """Ensures a USB power hub is configured on the device.

        If no infra-provided USB disconnector (e.g. DMC) is configured, falls
        back to `_ManualUsbPowerHub` which logs instructions for the user to
        unplug and replug the USB cable by hand.

        Args:
            device: Fuchsia device object.

        Returns:
            True if using manual USB unplug/replug, False if using an automated
            USB power hub.
        """
        if device.usb_power_hub is None:
            _LOGGER.info(
                "No USB disconnector (e.g. DMC) is configured for %s; "
                "falling back to manual USB cable unplug/replug.",
                device.device_name,
            )
            device.set_usb_power_hub(_ManualUsbPowerHub(device.device_name))
            return True
        return isinstance(device.usb_power_hub, _ManualUsbPowerHub)

    async def test_suspend_resume(self) -> None:
        """Must run on workbench products."""
        is_manual_usb = self._ensure_usb_power_hub(self.dut)
        timeout = (
            _MANUAL_SUSPEND_RESUME_TIMEOUT
            if is_manual_usb
            else _SUSPEND_RESUME_TIMEOUT
        )
        base_idle_duration = (
            _MANUAL_BASE_IDLE_DURATION
            if is_manual_usb
            else power.SUSPEND_RESUME_BASE_IDLE_DURATION
        )

        # TODO(https://fxbug.dev/519249679): Find a better way to
        # separate out product-specific test logic.
        #
        # On workbench, the display panel must be manually powered off
        # before suspend. Otherwise, the device will not suspend.
        self._set_display_power(self.dut, power_on=False)
        self._set_battery_manager_running(self.dut, running=False)
        try:
            await power.suspend_resume(
                self.dut,
                deadline=Deadline.from_timeout(timeout),
                base_idle_duration=base_idle_duration,
            )
        finally:
            self._set_battery_manager_running(self.dut, running=True)
            # On workbench, the display panel must be manually powered on after
            # resume.
            self._set_display_power(self.dut, power_on=True)

    async def test_no_suspend_on_usb(self) -> None:
        before_on_usb_idle_stats = await power.get_sag_suspend_stats(self.dut)

        # Then, idle a bit while plugged in to make sure we _don't_ suspend.
        await control_flows.sleep_for_duration(timedelta(seconds=60))

        while_on_usb_stats = (
            await power.get_sag_suspend_stats(self.dut)
            - before_on_usb_idle_stats
        )

        _LOGGER.info(
            f"Suspend stats during on-charger idle: \n{while_on_usb_stats}"
        )
        assert_equal(
            while_on_usb_stats.success_count,
            0,
            "SAG must not suspend during idle",
        )

        # NOTE(hjfreyer): These checks are meant to detect situations where the device sits in a
        # suspend attempt loop, but doesn't actually suspend. Checking that there were *no* attempts
        # to suspend seems like it could be too harsh and lead to flakes... but the threshold here
        # hasn't been tuned at all.
        assert_less(
            while_on_usb_stats.fail_count,
            10,
            "SAG attempted to suspend too many times while on USB",
        )
