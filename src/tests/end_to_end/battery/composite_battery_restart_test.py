#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Device-agnostic composite battery driver live restart E2E test.

Automates Test Case TC-06 (https://fxbug.dev/578654685) verifying live stop,
unmap, and composite rebinding of the active battery driver via
`ffx driver restart` and `fuchsia.hardware.power.battery.Service` FIDL
telemetry.

Pre-conditions:
  1. DUT is booted with `ffx` connected.
  2. A battery driver exposing `fuchsia.hardware.power.battery.Service` is
     bound.

Test Steps:
  1. Discover the bound battery driver moniker dynamically via
     `ffx component capability fuchsia.hardware.power.battery.Service` and
     resolve its component URL via `ffx component show <moniker>`.
  2. Record pre-restart `self.dut.battery.get_spec()` and
     `self.dut.battery.get_status()` telemetry.
  3. Trigger a live driver stop-and-start cycle via
     `ffx driver restart <driver_url>`.
  4. Reconnect and poll `self.dut.battery.get_spec()` and
     `self.dut.battery.get_status()` until the composite node finishes
     rebinding (up to 30 seconds).
  5. On Android/Starnix builds, query `adb shell dumpsys battery` to confirm
     live battery reporting resumed end-to-end.

Verification Criteria:
  - `ffx driver restart` completes with zero `collision` errors.
  - `self.dut.battery.get_spec()` and `get_status()` recover within the
    30-second rebind window with matching `spec` and valid capacity fields
    (plus non-regressing `cycle_count` when reported by the driver).
  - `adb shell dumpsys battery` reports a live `level:` reading after restart.
"""

import functools
import logging
from datetime import timedelta

import fuchsia_base_test
import utils
from honeydew import errors as honeydew_errors
from honeydew.device_classes import android_device
from honeydew.transports.ffx import types as ffx_types
from honeydew.utils import control_flows
from honeydew.utils.deadline import Deadline
from mobly import asserts, test_runner

_LOGGER: logging.Logger = logging.getLogger(__name__)

_REBIND_TIMEOUT: timedelta = timedelta(seconds=30)


class CompositeBatteryRestartTest(fuchsia_base_test.FuchsiaBaseTest):
    """End-to-end test for live composite battery driver restart via ffx."""

    async def setup_class(self) -> None:
        """Initializes DUT and refreshes FuchsiaController context."""
        await super().setup_class()
        self.dut.fuchsia_controller.create_context()

    async def test_composite_battery_driver_restart(self) -> None:
        """Restarts the bound battery driver and verifies FIDL recovery."""
        has_real_battery = utils.parse_bool_param(
            self.user_params, "has_real_battery", default=False
        )
        has_fake_battery = False
        try:
            _ = self.dut.fake_battery
            has_fake_battery = True
        except honeydew_errors.NotSupportedError:
            pass

        moniker = utils.discover_battery_moniker(self.dut)
        driver_url = utils.resolve_driver_url(self.dut, moniker)
        _LOGGER.info(
            "Discovered battery driver moniker=%s, url=%s", moniker, driver_url
        )

        spec_before = await self.dut.battery.get_spec()
        status_before = await self.dut.battery.get_status()
        _LOGGER.info(
            "Pre-restart battery spec=%s, status=%s",
            spec_before,
            status_before,
        )
        expect_cycle_count = status_before.cycle_count is not None
        asserts.assert_is_not_none(status_before.full_charge_capacity_uah)
        asserts.assert_is_not_none(status_before.remaining_capacity_uah)

        try:
            restart_out = self.dut.ffx.run(
                ["driver", "restart", driver_url],
                machine=ffx_types.MachineFormat.RAW,
            )
            _LOGGER.info("`ffx driver restart` output: %s", restart_out.strip())
        except honeydew_errors.HoneydewError as err:
            restart_out = str(err)
            lower_err = restart_out.lower()
            if any(
                token in lower_err
                for token in (
                    "connection to the device has been lost",
                    "invalid scope id",
                    "invalid network interface scope",
                    "invalid network interface id",
                    "timed out",
                )
            ):
                _LOGGER.info(
                    "USB transport bounced while restarting composite driver "
                    "%s (which also hosts usb-connector/TCPC); waiting for "
                    "device to reconnect...",
                    driver_url,
                )
            else:
                raise
        asserts.assert_not_in(
            "collision",
            restart_out.lower(),
            f"Unexpected driver host collision during restart: {restart_out}",
        )

        await self.dut.wait_for_online()
        self.dut.fuchsia_controller.create_context()

        spec_after, status_after = await control_flows.retry_until_deadline(
            task=functools.partial(
                utils.poll_rebound_fidl,
                self.dut,
                expect_cycle_count=expect_cycle_count,
            ),
            deadline=Deadline.from_timeout(_REBIND_TIMEOUT),
            retry_delay=timedelta(seconds=2),
        )
        _LOGGER.info(
            "Post-restart battery spec=%s, status=%s",
            spec_after,
            status_after,
        )
        asserts.assert_equal(spec_after, spec_before)
        if expect_cycle_count:
            assert status_before.cycle_count is not None
            assert status_after.cycle_count is not None
            asserts.assert_greater_equal(
                status_after.cycle_count,
                status_before.cycle_count,
                f"Expected cycle_count not to regress across driver restart: "
                f"before={status_before.cycle_count}, "
                f"after={status_after.cycle_count}",
            )
        assert status_after.full_charge_capacity_uah is not None
        assert status_after.remaining_capacity_uah is not None
        asserts.assert_greater(status_after.full_charge_capacity_uah, 0)
        if has_fake_battery or has_real_battery:
            asserts.assert_greater(status_after.remaining_capacity_uah, 0)
        else:
            asserts.assert_greater_equal(status_after.remaining_capacity_uah, 0)

        if isinstance(self.dut, android_device.AndroidDevice):
            dumpsys_out = self.dut.adb.run(["shell", "dumpsys", "battery"])
            asserts.assert_in(
                "level:",
                dumpsys_out,
                "Expected `adb shell dumpsys battery` to report live level",
            )


if __name__ == "__main__":
    test_runner.main()
