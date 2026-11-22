#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Device-agnostic Battery End-to-End Test."""

import logging
from datetime import timedelta

import fuchsia_base_test
import utils
from honeydew import errors as honeydew_errors
from honeydew.affordances.drivers.battery_manager.utils import (
    types as battery_types,
)
from honeydew.utils import control_flows
from honeydew.utils.deadline import Deadline
from mobly import asserts, test_runner

_LOGGER: logging.Logger = logging.getLogger(__name__)

_DISCHARGE_POLL_TIMEOUT: timedelta = timedelta(seconds=10)
_DISCHARGE_POLL_INTERVAL: timedelta = timedelta(milliseconds=500)
_DISCHARGE_MIN_VOLTAGE_UV: int = 3_500_000
_DISCHARGE_MIN_LEVEL_PERCENT: float = 10.0
_MAX_FULL_CAP_REBOOT_DRIFT_RATIO: float = 0.02
_MAX_REMAINING_CAP_REBOOT_DRIFT_RATIO: float = 0.05
_MAX_LEVEL_PERCENT_REBOOT_DRIFT: float = 5.0


class BatteryTest(fuchsia_base_test.FuchsiaBaseTest):
    """Tests `fuchsia.hardware.power.battery.Service` across device setups."""

    async def setup_class(self) -> None:
        await super().setup_class()
        self.dut.fuchsia_controller.create_context()
        # `DebugService` overrides are sticky across channel close per
        # `charger.fidl`; clear any stale override from an interrupted prior run.
        await utils.clear_charger_debug_control(self.dut, check=False)

    async def setup_test(self) -> None:
        await super().setup_test()
        self.battery = self.dut.battery
        self._has_real_battery: bool = utils.parse_bool_param(
            self.user_params, "has_real_battery", default=False
        )
        self._skip_reboot_test: bool = utils.parse_bool_param(
            self.user_params, "skip_reboot_test", default=False
        )
        self._charger_override_active: bool = False
        self.fake_battery = None
        try:
            self.fake_battery = self.dut.fake_battery
        except honeydew_errors.NotSupportedError:
            _LOGGER.info(
                "fake_battery affordance not present on %s; running against "
                "hardware battery driver (has_real_battery=%s, "
                "skip_reboot_test=%s)",
                self.dut.device_name,
                self._has_real_battery,
                self._skip_reboot_test,
            )

    async def teardown_test(self) -> None:
        if self.fake_battery is not None:
            try:
                self.fake_battery.set_charging()
            except Exception as err:  # pylint: disable=broad-except
                _LOGGER.warning(
                    "Failed to reset fake battery state in teardown: %s", err
                )
        if self._charger_override_active:
            await utils.clear_charger_debug_control(self.dut, check=False)
            self._charger_override_active = False
        await super().teardown_test()

    async def test_battery_get_spec(self) -> None:
        """Verify reading battery specification via get_spec."""
        _LOGGER.info("Calling get_spec on %s", self.dut.device_name)
        spec = await self.battery.get_spec()
        _LOGGER.info("Battery spec received: %s", spec)
        asserts.assert_is_not_none(spec, msg="Battery spec should not be None")
        asserts.assert_is_instance(
            spec,
            battery_types.BatterySpec,
            f"Expected battery_types.BatterySpec, got {type(spec)}",
        )
        if spec.design_capacity_uah is not None:
            asserts.assert_greater(
                spec.design_capacity_uah,
                0,
                "Expected positive design_capacity_uah in battery_types.BatterySpec",
            )

    async def test_get_status(self) -> None:
        """Verify reading battery telemetry status via get_status."""
        _LOGGER.info("Calling get_status on %s", self.dut.device_name)
        status = await self.battery.get_status()
        _LOGGER.info("Battery status received: %s", status)
        asserts.assert_is_not_none(
            status, msg="Battery status should not be None"
        )
        asserts.assert_is_instance(
            status,
            battery_types.BatteryStatus,
            f"Expected battery_types.BatteryStatus, got {type(status)}",
        )

    async def _poll_discharging_status(self) -> battery_types.BatteryStatus:
        """Polls `battery.get_status()` until `current_ua` is negative."""
        status = await self.battery.get_status()
        if status.current_ua is None or status.current_ua >= 0:
            raise control_flows.RetriableError(
                f"Battery current_ua not yet negative after forcing "
                f"DISCHARGING: {status}"
            )
        return status

    async def test_set_discharging_status(self) -> None:
        """Verify forcing battery discharge on emulator or real-battery hardware."""
        if self.fake_battery is not None:
            _LOGGER.info(
                "Setting fake battery to discharging on %s",
                self.dut.device_name,
            )
            self.fake_battery.set_discharging(current_ua=-500000)
            status = await self.battery.get_status()
            _LOGGER.info("Battery status received: %s", status)
            asserts.assert_is_not_none(status.current_ua)
            assert status.current_ua is not None
            asserts.assert_less(
                status.current_ua,
                0,
                f"Expected negative discharging current_ua, got {status.current_ua}",
            )
            return

        if not self._has_real_battery:
            asserts.skip(
                "Skipping discharging test when has_real_battery=False: "
                "disabling the buck converter on a battery eliminator "
                "without a physical cell can brown out VSYS."
            )

        if (
            utils.try_discover_service_moniker(
                self.dut, utils.CHARGER_DEBUG_SERVICE_CAPABILITY
            )
            is None
        ):
            asserts.skip(
                f"Skipping discharging test: "
                f"`{utils.CHARGER_DEBUG_SERVICE_CAPABILITY}` is not available "
                f"on {self.dut.device_name} (e.g. non-eng build)."
            )

        pre_status = await self.battery.get_status()
        asserts.assert_true(
            pre_status.present is True,
            f"Expected battery present=True before forcing DISCHARGING, "
            f"got {pre_status}",
        )
        asserts.assert_true(
            pre_status.voltage_uv is not None
            or pre_status.level_percent is not None,
            f"Expected voltage_uv or level_percent telemetry before forcing "
            f"DISCHARGING, got {pre_status}",
        )
        if (
            pre_status.voltage_uv is not None
            and pre_status.voltage_uv < _DISCHARGE_MIN_VOLTAGE_UV
        ):
            asserts.skip(
                f"Skipping discharging test: battery voltage "
                f"{pre_status.voltage_uv} uV is below safe floor "
                f"{_DISCHARGE_MIN_VOLTAGE_UV} uV."
            )
        if (
            pre_status.level_percent is not None
            and pre_status.level_percent < _DISCHARGE_MIN_LEVEL_PERCENT
        ):
            asserts.skip(
                f"Skipping discharging test: battery level_percent "
                f"{pre_status.level_percent:.2f}% is below safe floor "
                f"{_DISCHARGE_MIN_LEVEL_PERCENT:.1f}%."
            )

        _LOGGER.info(
            "Setting charger OperatingMode.DISCHARGING via DebugService on %s "
            "(pre_status=%s)",
            self.dut.device_name,
            pre_status,
        )
        self._charger_override_active = True
        try:
            await utils.set_charger_discharging(self.dut)
            status = await control_flows.retry_until_deadline(
                task=self._poll_discharging_status,
                deadline=Deadline.from_timeout(_DISCHARGE_POLL_TIMEOUT),
                retry_delay=_DISCHARGE_POLL_INTERVAL,
            )
            _LOGGER.info("Discharging battery status received: %s", status)
            assert status.current_ua is not None
            asserts.assert_less(
                status.current_ua,
                0,
                f"Expected negative discharging current_ua, got {status.current_ua}",
            )
        finally:
            await utils.clear_charger_debug_control(self.dut, check=True)
            self._charger_override_active = False

    async def test_battery_status_across_reboot(self) -> None:
        """Verify battery spec and persistent status fields across a reboot."""
        if self._skip_reboot_test:
            asserts.skip(
                "Skipping generic cross-reboot battery check on real-battery "
                "bench (covered alongside MAX77779 NVM checks by "
                "learned_battery_parameters in a single reboot)."
            )

        _LOGGER.info(
            "Verifying initial battery spec and status on %s",
            self.dut.device_name,
        )
        spec_before = await self.battery.get_spec()
        status_before = await self.battery.get_status()
        _LOGGER.info(
            "Battery before reboot: spec=%s, status=%s",
            spec_before,
            status_before,
        )
        asserts.assert_is_not_none(
            spec_before, msg="Battery spec before reboot should not be None"
        )
        asserts.assert_is_not_none(
            status_before, msg="Battery status before reboot should not be None"
        )
        asserts.assert_is_not_none(
            status_before.full_charge_capacity_uah,
            "Expected non-None full_charge_capacity_uah before reboot",
        )
        assert status_before.full_charge_capacity_uah is not None
        asserts.assert_greater(
            status_before.full_charge_capacity_uah,
            0,
            "Expected positive full_charge_capacity_uah before reboot",
        )
        asserts.assert_is_not_none(
            status_before.remaining_capacity_uah,
            "Expected non-None remaining_capacity_uah before reboot",
        )
        assert status_before.remaining_capacity_uah is not None

        _LOGGER.info("Rebooting device %s...", self.dut.device_name)
        await self.dut.reboot()
        _LOGGER.info("Device reboot completed successfully")

        spec_after = await self.battery.get_spec()
        status_after = await self.battery.get_status()
        _LOGGER.info(
            "Battery after reboot: spec=%s, status=%s",
            spec_after,
            status_after,
        )
        asserts.assert_equal(
            spec_after,
            spec_before,
            f"Expected battery spec to persist across reboot: "
            f"before={spec_before}, after={spec_after}",
        )
        asserts.assert_equal(
            status_after.present,
            status_before.present,
            "Expected battery present flag to match across reboot",
        )
        if status_before.cycle_count is not None:
            asserts.assert_is_not_none(
                status_after.cycle_count,
                "Expected non-None cycle_count after reboot when set before reboot",
            )
            assert status_after.cycle_count is not None
            asserts.assert_greater_equal(
                status_after.cycle_count,
                status_before.cycle_count,
                f"Expected cycle_count not to regress across reboot: "
                f"before={status_before.cycle_count}, "
                f"after={status_after.cycle_count}",
            )
        asserts.assert_is_not_none(
            status_after.full_charge_capacity_uah,
            "Expected non-None full_charge_capacity_uah after reboot",
        )
        assert status_after.full_charge_capacity_uah is not None
        asserts.assert_greater(
            status_after.full_charge_capacity_uah,
            0,
            "Expected positive full_charge_capacity_uah after reboot",
        )
        max_full_cap_drift = int(
            status_before.full_charge_capacity_uah
            * _MAX_FULL_CAP_REBOOT_DRIFT_RATIO
        )
        asserts.assert_less_equal(
            abs(
                status_after.full_charge_capacity_uah
                - status_before.full_charge_capacity_uah
            ),
            max_full_cap_drift,
            f"Expected full_charge_capacity_uah to stay within 2% across "
            f"reboot: before={status_before.full_charge_capacity_uah}, "
            f"after={status_after.full_charge_capacity_uah}",
        )
        asserts.assert_is_not_none(
            status_after.remaining_capacity_uah,
            "Expected non-None remaining_capacity_uah after reboot",
        )
        assert status_after.remaining_capacity_uah is not None
        if self.fake_battery is not None or self._has_real_battery:
            asserts.assert_greater(
                status_after.remaining_capacity_uah,
                0,
                "Expected positive remaining_capacity_uah after reboot",
            )
            max_rem_cap_drift = int(
                status_before.full_charge_capacity_uah
                * _MAX_REMAINING_CAP_REBOOT_DRIFT_RATIO
            )
            asserts.assert_less_equal(
                abs(
                    status_after.remaining_capacity_uah
                    - status_before.remaining_capacity_uah
                ),
                max_rem_cap_drift,
                f"Expected remaining_capacity_uah to stay within 5% of full "
                f"capacity across reboot: "
                f"before={status_before.remaining_capacity_uah}, "
                f"after={status_after.remaining_capacity_uah}",
            )
            if status_before.level_percent is not None:
                asserts.assert_is_not_none(
                    status_after.level_percent,
                    "Expected non-None level_percent after reboot",
                )
                assert status_after.level_percent is not None
                asserts.assert_less_equal(
                    abs(
                        status_after.level_percent - status_before.level_percent
                    ),
                    _MAX_LEVEL_PERCENT_REBOOT_DRIFT,
                    f"Expected level_percent to stay within "
                    f"{_MAX_LEVEL_PERCENT_REBOOT_DRIFT}% across reboot: "
                    f"before={status_before.level_percent}, "
                    f"after={status_after.level_percent}",
                )
        else:
            asserts.assert_greater_equal(
                status_after.remaining_capacity_uah,
                0,
                "Expected non-negative remaining_capacity_uah on battery eliminator",
            )


if __name__ == "__main__":
    test_runner.main()
