#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""On-Device Power Meter (ODPM) telemetry, boot delay, and suspend hooks test.

Automates Test Case TC-10 (https://fxbug.dev/578655470) validating the Iris
On-Device Power Meter (`odpm`) driver across its 20-second post-boot periodic
sampling delay, rail power/current/energy FIDL telemetry via `odpm_util`,
`powerstats` integration, and system suspend/resume hooks.

Pre-conditions:
  1. Iris AndroidDevice DUT is booted with both `ffx` and `adb` connected.

Test Steps:
  1. Check `/proc/uptime` via ADB and wait if needed so the DUT is past the
     20-second initial ODPM periodic reading delay window.
  2. Discover the bound ODPM driver moniker via
     `ffx component capability fuchsia.hardware.google.odpm.Service`.
  3. Query `odpm_util rails enabled`, `odpm_util read` (W),
     `odpm_util read -c` (A), and `odpm_util read -e` (J) via
     `ffx component explore <moniker> -c "..."` and verify 32 rail telemetry
     readings with non-negative values and advancing accumulated energy.
  4. Verify Android `adb shell dumpsys powerstats` reports all 32 ODPM rail
     channels under `available Channels`.
  5. Exercise idle suspend/resume and verify ODPM `root/power_management`
     Inspect `suspend_count` and `resume_count` advance and `odpm_util read` /
     `odpm_util read -e` remain healthy afterward.

Verification Criteria:
  - `fuchsia.hardware.google.odpm.Service` is exposed by the ODPM driver.
  - `odpm_util` reports 32 enabled rails with non-negative (`>= 0.0`) power (W),
    current (A), and energy (J) readings, at least one positive (`> 0.0`) rail,
    and monotonically advancing total energy (J).
  - `adb shell dumpsys powerstats` lists all 32 ODPM rail channels.
  - ODPM `root/power_management` Inspect `suspend_count` and `resume_count`
    both increment across suspend/resume, and ODPM remains responsive afterward.
"""

import logging

import fuchsia_base_test
import utils
from honeydew.device_classes import android_device
from mobly import asserts, test_runner

_LOGGER: logging.Logger = logging.getLogger(__name__)

_SUSPEND_DURATION_SECS: int = 15


class OdpmTelemetryTest(fuchsia_base_test.FuchsiaBaseTest):
    """End-to-end test for ODPM rail telemetry, boot window, and suspend."""

    dut: android_device.AndroidDevice

    async def setup_class(self) -> None:
        """Verifies the DUT is an AndroidDevice with active ADB transport."""
        await super().setup_class()
        assert isinstance(self.dut, android_device.AndroidDevice)
        _ = self.dut.adb

    async def test_odpm_power_energy_and_suspend_hooks(self) -> None:
        """Verifies ODPM FIDL rail telemetry, powerstats, and suspend hooks."""
        # Step 1: Ensure we are past the 20-second post-boot ODPM delay window.
        await utils.ensure_past_boot_delay(self.dut)

        # Step 2: Discover the active ODPM driver moniker by capability.
        odpm_moniker = utils.discover_odpm_moniker(self.dut)
        _LOGGER.info("Discovered ODPM driver moniker: %s", odpm_moniker)

        # Step 3: Query enabled rails and live power (W), current (A), and
        # accumulated energy (J) via `odpm_util`.
        rails_out = await utils.run_odpm_util(
            self.dut, odpm_moniker, "rails enabled"
        )
        asserts.assert_in(
            "cpu_big",
            rails_out,
            f"Expected `cpu_big` in `odpm_util rails enabled`:\n{rails_out}",
        )

        power_readings = utils.parse_odpm_readings(
            await utils.run_odpm_util(self.dut, odpm_moniker, "read")
        )
        current_readings = utils.parse_odpm_readings(
            await utils.run_odpm_util(self.dut, odpm_moniker, "read -c")
        )
        energy_before = utils.parse_odpm_readings(
            await utils.run_odpm_util(self.dut, odpm_moniker, "read -e")
        )
        _LOGGER.info(
            "ODPM rail counts: power=%d, current=%d, energy=%d",
            len(power_readings),
            len(current_readings),
            len(energy_before),
        )
        # TODO(https://fxbug.dev/578655470): Non-Iris boards may expose a
        # different number of enabled ODPM rails than 32.
        asserts.assert_equal(
            len(power_readings),
            utils.EXPECTED_ODPM_RAIL_COUNT,
            f"Expected {utils.EXPECTED_ODPM_RAIL_COUNT} rail power (W) "
            f"readings from `odpm_util read`, got {len(power_readings)}",
        )
        asserts.assert_equal(
            len(current_readings),
            utils.EXPECTED_ODPM_RAIL_COUNT,
            f"Expected {utils.EXPECTED_ODPM_RAIL_COUNT} rail current (A) "
            f"readings from `odpm_util read -c`, got {len(current_readings)}",
        )
        asserts.assert_equal(
            len(energy_before),
            utils.EXPECTED_ODPM_RAIL_COUNT,
            f"Expected {utils.EXPECTED_ODPM_RAIL_COUNT} rail energy (J) "
            f"readings from `odpm_util read -e`, got {len(energy_before)}",
        )
        asserts.assert_true(
            all(v >= 0.0 for v in power_readings.values())
            and any(v > 0.0 for v in power_readings.values()),
            f"Expected non-negative rail power (W) readings with at least one "
            f"positive rail, got {power_readings}",
        )
        asserts.assert_true(
            all(v >= 0.0 for v in current_readings.values()),
            f"Expected non-negative rail current (A) readings, got {current_readings}",
        )
        asserts.assert_true(
            all(v >= 0.0 for v in energy_before.values())
            and any(v > 0.0 for v in energy_before.values()),
            f"Expected non-negative rail energy (J) readings with at least one "
            f"positive rail, got {energy_before}",
        )

        # Step 4: Verify Android `dumpsys powerstats` integration exposes all
        # 32 ODPM rail channels.
        powerstats_out = self.dut.adb.run(["shell", "dumpsys", "powerstats"])
        powerstats_channels = utils.parse_powerstats_channels(powerstats_out)
        asserts.assert_equal(
            len(powerstats_channels),
            utils.EXPECTED_ODPM_RAIL_COUNT,
            f"Expected {utils.EXPECTED_ODPM_RAIL_COUNT} ODPM channels in "
            f"`adb shell dumpsys powerstats`, got {len(powerstats_channels)}",
        )
        asserts.assert_equal(
            {subsystem for _, subsystem in powerstats_channels.values()},
            set(power_readings.keys()),
            "Expected `dumpsys powerstats` channel subsystems to match active "
            "ODPM rails",
        )

        # Step 5: Exercise idle suspend/resume and verify ODPM suspend/resume
        # hooks increment in Inspect and ODPM remains healthy afterward.
        suspend_before, resume_before = utils.get_odpm_suspend_resume_counts(
            self.dut, odpm_moniker
        )
        _LOGGER.info(
            "ODPM power_management counts before suspend: "
            "suspend_count=%d, resume_count=%d",
            suspend_before,
            resume_before,
        )

        await utils.suspend_and_resume(
            self.dut, duration_secs=_SUSPEND_DURATION_SECS
        )

        power_after = utils.parse_odpm_readings(
            await utils.run_odpm_util(self.dut, odpm_moniker, "read")
        )
        energy_after = utils.parse_odpm_readings(
            await utils.run_odpm_util(self.dut, odpm_moniker, "read -e")
        )
        suspend_after, resume_after = utils.get_odpm_suspend_resume_counts(
            self.dut, odpm_moniker
        )
        _LOGGER.info(
            "ODPM power_management counts after suspend: "
            "suspend_count=%d, resume_count=%d",
            suspend_after,
            resume_after,
        )
        asserts.assert_greater(
            suspend_after,
            suspend_before,
            "Expected ODPM `root/power_management:suspend_count` to increment "
            "across suspend/resume",
        )
        asserts.assert_greater(
            resume_after,
            resume_before,
            "Expected ODPM `root/power_management:resume_count` to increment "
            "across suspend/resume",
        )
        asserts.assert_equal(
            len(power_after),
            utils.EXPECTED_ODPM_RAIL_COUNT,
            f"Expected {utils.EXPECTED_ODPM_RAIL_COUNT} rail power (W) "
            f"readings after suspend, got {len(power_after)}",
        )
        asserts.assert_equal(
            len(energy_after),
            utils.EXPECTED_ODPM_RAIL_COUNT,
            f"Expected {utils.EXPECTED_ODPM_RAIL_COUNT} rail energy (J) "
            f"readings after suspend, got {len(energy_after)}",
        )
        asserts.assert_true(
            all(v >= 0.0 for v in power_after.values())
            and any(v > 0.0 for v in power_after.values()),
            "Expected ODPM rail power (W) readings to remain non-negative "
            f"with at least one positive rail after suspend, got {power_after}",
        )
        asserts.assert_true(
            all(v >= 0.0 for v in energy_after.values()),
            "Expected ODPM rail energy (J) readings to remain non-negative "
            f"after suspend, got {energy_after}",
        )
        asserts.assert_greater_equal(
            sum(energy_after.values()),
            sum(energy_before.values()),
            "Expected total ODPM accumulated energy (J) to be non-decreasing",
        )


if __name__ == "__main__":
    test_runner.main()
