#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Iris PowerStats display residency and Starnix cpuidle (C1/C2) E2E test.

Automates Test Case TC-11 (https://fxbug.dev/578655633) verifying the Iris
PowerStats Display State Residency watcher across display ON/OFF intervals and
Starnix `/sys/devices/system/cpu/cpu0/cpuidle` C1/C2 state residency counters.

Pre-conditions:
  1. Iris AndroidDevice DUT is booted with `adb` connected.

Test Steps:
  1. Read baseline `name`, `time` (microseconds), and `usage` counters for
     `state0` (C1) and `state1` (C2) under
     `/sys/devices/system/cpu/cpu0/cpuidle`.
  2. Temporarily raise `screen_off_timeout`, wake the display
     (`KEYCODE_WAKEUP`), dismiss Keyguard (`wm dismiss-keyguard`), and under
     root (`with self.dut.adb.use_adb_root():`) read baseline live `Display`
     state residencies from
     `dumpsys android.hardware.power.stats.IPowerStats/default`, dwell 15
     seconds in the ON state, and re-read `Display` state residencies.
  3. Put the display to sleep (`KEYCODE_SLEEP`), dwell 15 seconds in the
     OFF/DOZE state, re-read live `Display` state residencies from
     `dumpsys android.hardware.power.stats.IPowerStats/default`, and restore
     `KEYCODE_WAKEUP` and `screen_off_timeout` afterward.
  4. Re-read `state0` and `state1` cpuidle residency `time` and `usage`
     counters after the ON/OFF dwell window.

Verification Criteria:
  - `state0` and `state1` cpuidle names match `C1` and `C2`.
  - `dumpsys android.hardware.power.stats.IPowerStats/default` exposes the
    `Display` entity with `OFF`, `NBM:<w>x<h>@<fps>`, and `LPM:<w>x<h>@1`
    states registered by `DisplayStateResidencyWatcher`.
  - Live `NBM:` residency duration advances across the 15-second ON dwell, and
    `OFF` (or `LPM:` when `doze_always_on` is enabled) residency duration and
    entry count advance across the 15-second sleep dwell.
  - Combined C1 + C2 cpuidle residency `time` and `usage` strictly advance
    across the idle dwell window.
"""

import logging
from datetime import timedelta

import fuchsia_base_test
import utils
from honeydew import errors
from honeydew.device_classes import android_device
from honeydew.utils import control_flows
from mobly import asserts, test_runner

_LOGGER: logging.Logger = logging.getLogger(__name__)

_DISPLAY_DWELL: timedelta = timedelta(seconds=15)
_WAKE_SETTLE_DELAY: timedelta = timedelta(seconds=2)
_TEST_SCREEN_OFF_TIMEOUT_MS: str = "600000"


class PowerstatsAndCpuidleTest(fuchsia_base_test.FuchsiaBaseTest):
    """End-to-end test for Iris PowerStats display residency and cpuidle."""

    dut: android_device.AndroidDevice
    _display_asleep: bool = False
    _saved_screen_off_timeout: str | None = None

    async def setup_class(self) -> None:
        """Verifies the DUT is an AndroidDevice with active ADB transport."""
        await super().setup_class()
        self.dut.fuchsia_controller.create_context()
        assert isinstance(self.dut, android_device.AndroidDevice)
        _ = self.dut.adb
        self._display_asleep = False
        self._saved_screen_off_timeout = None
        self._restore_display_awake(check=False)

    async def teardown_test(self) -> None:
        """Restores display timeout and wake state if interrupted."""
        if self._saved_screen_off_timeout is not None:
            self._restore_screen_off_timeout(check=False)
        if self._display_asleep:
            self._restore_display_awake(check=False)
        await super().teardown_test()

    def _restore_display_awake(self, *, check: bool) -> None:
        """Wakes the display via `KEYCODE_WAKEUP` and clears `_display_asleep`."""
        try:
            self.dut.adb.run(["shell", "input", "keyevent", "KEYCODE_WAKEUP"])
            self._display_asleep = False
        except errors.HoneydewError as err:
            if check:
                raise
            _LOGGER.warning(
                "Best-effort KEYCODE_WAKEUP cleanup failed: %s", err
            )

    def _restore_screen_off_timeout(self, *, check: bool) -> None:
        """Restores `settings system screen_off_timeout` to its saved value."""
        saved = self._saved_screen_off_timeout
        if saved is None:
            return
        if saved.isdigit():
            cmd = [
                "shell",
                "settings",
                "put",
                "system",
                "screen_off_timeout",
                saved,
            ]
        else:
            cmd = [
                "shell",
                "settings",
                "delete",
                "system",
                "screen_off_timeout",
            ]
        try:
            self.dut.adb.run(cmd)
            self._saved_screen_off_timeout = None
        except errors.HoneydewError as err:
            if check:
                raise
            _LOGGER.warning(
                "Best-effort screen_off_timeout cleanup failed: %s", err
            )

    async def test_display_residency_and_cpuidle_counters(self) -> None:
        """Toggles display ON/OFF/ON and verifies IPowerStats and cpuidle."""
        # Step 1: Capture baseline Starnix cpuidle C1 (`state0`) and C2
        # (`state1`) names, residency durations, and entry counts.
        s0_name, s0_time_before, s0_usage_before = utils.read_cpuidle_state(
            self.dut, "state0"
        )
        s1_name, s1_time_before, s1_usage_before = utils.read_cpuidle_state(
            self.dut, "state1"
        )
        _LOGGER.info(
            "Initial cpuidle: %s(time=%d, usage=%d), %s(time=%d, usage=%d)",
            s0_name,
            s0_time_before,
            s0_usage_before,
            s1_name,
            s1_time_before,
            s1_usage_before,
        )
        asserts.assert_in(
            "C1",
            s0_name.upper(),
            f"Expected C1 cpuidle state0 name, got {s0_name!r}",
        )
        asserts.assert_in(
            "C2",
            s1_name.upper(),
            f"Expected C2 cpuidle state1 name, got {s1_name!r}",
        )

        # Check whether Always-On Display (`doze_always_on`) is enabled (or
        # unset/"null", falling back to the device overlay), which routes
        # `KEYCODE_SLEEP` to `PowerMode::DOZE` -> `LPM:<w>x<h>@1` instead of
        # `PowerMode::OFF` -> `OFF`.
        doze_always_on_raw = self.dut.adb.run(
            ["shell", "settings", "get", "secure", "doze_always_on"]
        ).strip()
        doze_always_on = doze_always_on_raw != "0"
        _LOGGER.info(
            "doze_always_on=%s (raw=%r)", doze_always_on, doze_always_on_raw
        )

        # Steps 2 & 3: Temporarily extend `screen_off_timeout` and dismiss
        # Keyguard so the display stays in `NBM:` across the 15s ON dwell, then
        # query live `IPowerStats/default` Display State Residency across 15s
        # ON (`KEYCODE_WAKEUP`) and 15s OFF/DOZE (`KEYCODE_SLEEP`) dwell windows
        # under root. (`teardown_test()` restores `screen_off_timeout` and
        # `KEYCODE_WAKEUP` if interrupted.)
        self._saved_screen_off_timeout = self.dut.adb.run(
            ["shell", "settings", "get", "system", "screen_off_timeout"]
        ).strip()
        self.dut.adb.run(
            [
                "shell",
                "settings",
                "put",
                "system",
                "screen_off_timeout",
                _TEST_SCREEN_OFF_TIMEOUT_MS,
            ]
        )
        self.dut.adb.run(["shell", "input", "keyevent", "KEYCODE_WAKEUP"])
        self.dut.adb.run(["shell", "wm", "dismiss-keyguard"])
        await control_flows.sleep_for_duration(_WAKE_SETTLE_DELAY)

        with self.dut.adb.use_adb_root():
            residency_before_on = utils.read_display_state_residencies(self.dut)
            _LOGGER.info(
                "Display residency before ON dwell: %s",
                residency_before_on,
            )
            nbm_states = [
                name for name in residency_before_on if name.startswith("NBM:")
            ]
            lpm_states = [
                name for name in residency_before_on if name.startswith("LPM:")
            ]
            asserts.assert_in(
                "OFF",
                residency_before_on,
                "Expected `OFF` state registered under `Display` in "
                f"`dumpsys {utils.POWERSTATS_HAL_SERVICE}`, got "
                f"{list(residency_before_on.keys())}",
            )
            asserts.assert_greater(
                len(nbm_states),
                0,
                "Expected at least one `NBM:<w>x<h>@<fps>` state registered "
                f"under `Display` in `dumpsys {utils.POWERSTATS_HAL_SERVICE}`, "
                f"got {list(residency_before_on.keys())}",
            )
            asserts.assert_greater(
                len(lpm_states),
                0,
                "Expected at least one `LPM:<w>x<h>@1` state registered "
                f"under `Display` in `dumpsys {utils.POWERSTATS_HAL_SERVICE}`, "
                f"got {list(residency_before_on.keys())}",
            )

            await control_flows.sleep_for_duration(_DISPLAY_DWELL)
            residency_after_on = utils.read_display_state_residencies(self.dut)
            _LOGGER.info(
                "Display residency after ON dwell: %s",
                residency_after_on,
            )

            self._display_asleep = True
            self.dut.adb.run(["shell", "input", "keyevent", "KEYCODE_SLEEP"])
            await control_flows.sleep_for_duration(_DISPLAY_DWELL)
            residency_after_off = utils.read_display_state_residencies(self.dut)
            _LOGGER.info(
                "Display residency after OFF dwell: %s",
                residency_after_off,
            )

        self._restore_display_awake(check=True)
        self._restore_screen_off_timeout(check=True)

        # Verify `NBM:` residency time advanced during the 15s ON dwell.
        nbm_time_before = sum(
            residency_before_on[s].total_time_ms for s in nbm_states
        )
        nbm_time_after = sum(
            residency_after_on[s].total_time_ms for s in nbm_states
        )
        asserts.assert_greater(
            nbm_time_after,
            nbm_time_before,
            "Expected `Display` `NBM:` state residency time (ms) to advance "
            f"across ON dwell (before={nbm_time_before}, after={nbm_time_after})",
        )

        # Verify `OFF` (or `LPM:` when `doze_always_on` is enabled) residency
        # time and entry count advanced during the 15s sleep dwell.
        if not doze_always_on:
            off_before = residency_after_on["OFF"]
            off_after = residency_after_off["OFF"]
            asserts.assert_greater(
                off_after.total_time_ms,
                off_before.total_time_ms,
                "Expected `Display` `OFF` state residency time (ms) to advance "
                f"across OFF dwell (before={off_before.total_time_ms}, "
                f"after={off_after.total_time_ms})",
            )
            asserts.assert_greater(
                off_after.total_entry_count,
                off_before.total_entry_count,
                "Expected `Display` `OFF` state entry count to increment "
                f"across OFF dwell (before={off_before.total_entry_count}, "
                f"after={off_after.total_entry_count})",
            )
        else:
            sleep_states = ["OFF", *lpm_states]
            sleep_time_before = sum(
                residency_after_on[s].total_time_ms for s in sleep_states
            )
            sleep_time_after = sum(
                residency_after_off[s].total_time_ms for s in sleep_states
            )
            sleep_entries_before = sum(
                residency_after_on[s].total_entry_count for s in sleep_states
            )
            sleep_entries_after = sum(
                residency_after_off[s].total_entry_count for s in sleep_states
            )
            asserts.assert_greater(
                sleep_time_after,
                sleep_time_before,
                "Expected `Display` `OFF`/`LPM:` state residency time (ms) to "
                f"advance across sleep dwell with doze_always_on={doze_always_on_raw!r} "
                f"(before={sleep_time_before}, after={sleep_time_after})",
            )
            asserts.assert_greater(
                sleep_entries_after,
                sleep_entries_before,
                "Expected `Display` `OFF`/`LPM:` state entry count to "
                f"increment across sleep dwell with doze_always_on={doze_always_on_raw!r} "
                f"(before={sleep_entries_before}, after={sleep_entries_after})",
            )

        # Step 4: Re-sample C1/C2 cpuidle counters and assert monotonic growth.
        _, s0_time_after, s0_usage_after = utils.read_cpuidle_state(
            self.dut, "state0"
        )
        _, s1_time_after, s1_usage_after = utils.read_cpuidle_state(
            self.dut, "state1"
        )
        _LOGGER.info(
            "Post-dwell cpuidle: state0(time=%d, usage=%d), "
            "state1(time=%d, usage=%d)",
            s0_time_after,
            s0_usage_after,
            s1_time_after,
            s1_usage_after,
        )

        asserts.assert_greater(
            s0_time_after + s1_time_after,
            s0_time_before + s1_time_before,
            "Expected cpuidle residency time to advance across idle window",
        )
        asserts.assert_greater(
            s0_usage_after + s1_usage_after,
            s0_usage_before + s1_usage_before,
            "Expected cpuidle usage count to advance across idle window",
        )


if __name__ == "__main__":
    test_runner.main()
