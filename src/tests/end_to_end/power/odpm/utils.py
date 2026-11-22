#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Shared helper utilities for ODPM and PowerStats End-to-End tests."""

import logging
import re
from datetime import timedelta
from typing import NamedTuple

from honeydew import errors
from honeydew.affordances.power.system_power_state_controller import (
    system_power_state_controller as spsc_interface,
)
from honeydew.affordances.power.system_power_state_controller import (
    system_power_state_controller_using_usb_cli as usb_cli_controller,
)
from honeydew.device_classes import android_device
from honeydew.transports.ffx import types as ffx_types
from honeydew.utils import control_flows
from honeydew.utils import power as honeydew_power
from honeydew.utils.deadline import Deadline
from mobly import asserts

_LOGGER: logging.Logger = logging.getLogger(__name__)

ODPM_SERVICE_CAPABILITY: str = "fuchsia.hardware.google.odpm.Service"
# TODO(https://fxbug.dev/578655470): Non-Iris boards may expose a different
# number of enabled ODPM rails than 32.
EXPECTED_ODPM_RAIL_COUNT: int = 32
MIN_BOOT_UPTIME_SECS: float = 22.0
ODPM_CLI_TIMEOUT: timedelta = timedelta(seconds=10)
CPUIDLE_BASE: str = "/sys/devices/system/cpu/cpu0/cpuidle"
POWERSTATS_HAL_SERVICE: str = "android.hardware.power.stats.IPowerStats/default"
_SCREEN_OFF_SETTLE_DURATION: timedelta = timedelta(seconds=3)
_SUSPEND_RESUME_TIMEOUT: timedelta = timedelta(minutes=1)

_RAIL_READING_RE = re.compile(
    r"Rail\s+(\S+)\s+\[[^\]]+\]:\s+(-?\d+(?:\.\d+)?)\s+([WJA])"
)
_POWERSTATS_CHANNEL_RE = re.compile(
    r"ChannelId:\s*(\d+),\s*ChannelName:\s*(\S+),\s*ChannelSubsystem:\s*(\S+)"
)
_DISPLAY_STATE_RESIDENCY_RE = re.compile(
    r"^\s*Display\s+(OFF|(?:NBM|LPM):\d+x\d+@\d+)\s+"
    r"(\d+)\s+ms\s+(\d+)\s+(\d+)\s+ms\s*$",
    re.MULTILINE,
)


class DisplayStateResidency(NamedTuple):
    """Single state residency row for the `Display` PowerStats entity."""

    total_time_ms: int
    total_entry_count: int
    last_entry_timestamp_ms: int


def parse_odpm_readings(output: str) -> dict[str, float]:
    """Parses `odpm_util read` output into a `{rail_name: value}` map."""
    readings: dict[str, float] = {}
    for match in _RAIL_READING_RE.finditer(output):
        rail_name = match.group(1)
        value = float(match.group(2))
        readings[rail_name] = value
    return readings


def parse_powerstats_channels(output: str) -> dict[int, tuple[str, str]]:
    """Parses `dumpsys powerstats` channel lines into `{id: (name, subsystem)}`."""
    channels: dict[int, tuple[str, str]] = {}
    for match in _POWERSTATS_CHANNEL_RE.finditer(output):
        channel_id = int(match.group(1))
        channel_name = match.group(2)
        subsystem = match.group(3)
        channels[channel_id] = (channel_name, subsystem)
    return channels


def discover_odpm_moniker(dut: android_device.AndroidDevice) -> str:
    """Discovers the ODPM driver moniker by FIDL service capability."""
    output = dut.ffx.run(
        ["component", "capability", ODPM_SERVICE_CAPABILITY],
        timeout=ODPM_CLI_TIMEOUT.total_seconds(),
        machine=ffx_types.MachineFormat.RAW,
    )
    for raw_line in output.splitlines():
        line = raw_line.strip()
        if (
            f"declared capability `{ODPM_SERVICE_CAPABILITY}`" in line
            or f"exposed `{ODPM_SERVICE_CAPABILITY}` from self to parent"
            in line
            or f"exposed capability `{ODPM_SERVICE_CAPABILITY}` from self to parent"
            in line
        ) and "devfs_driver" not in line:
            parts = line.split("`")
            if len(parts) >= 2 and parts[1] != ODPM_SERVICE_CAPABILITY:
                return parts[1]
    asserts.fail(
        f"No ODPM driver declaring `{ODPM_SERVICE_CAPABILITY}` found in:\n"
        f"{output}"
    )
    return ""


async def run_odpm_util(
    dut: android_device.AndroidDevice, moniker: str, subcmd: str
) -> str:
    """Runs an `odpm_util` command inside the ODPM component namespace."""
    deadline = Deadline.from_timeout(ODPM_CLI_TIMEOUT)

    async def _attempt() -> str:
        remaining = deadline.remaining_seconds()
        attempt_timeout = (
            max(1.0, remaining)
            if remaining is not None
            else ODPM_CLI_TIMEOUT.total_seconds()
        )
        try:
            return dut.ffx.run(
                [
                    "component",
                    "explore",
                    moniker,
                    "-c",
                    f"odpm_util {subcmd}",
                ],
                timeout=attempt_timeout,
                machine=ffx_types.MachineFormat.RAW,
            )
        except errors.HoneydewError as err:
            if "Status(UNAVAILABLE)" in str(err):
                raise control_flows.RetriableError(
                    f"Transient CPM mailbox busy for `odpm_util {subcmd}`: {err}"
                ) from err
            raise control_flows.RetryAbortingError(str(err)) from err

    return await control_flows.retry_until_deadline(
        task=_attempt,
        deadline=deadline,
        retry_delay=timedelta(seconds=1),
    )


async def ensure_past_boot_delay(dut: android_device.AndroidDevice) -> None:
    """Waits past the 20s initial ODPM periodic reading delay if needed."""
    uptime_raw = dut.adb.run(["shell", "cat", "/proc/uptime"]).strip()
    try:
        uptime_secs = float(uptime_raw.split()[0])
    except (IndexError, ValueError):
        uptime_secs = MIN_BOOT_UPTIME_SECS

    if uptime_secs < MIN_BOOT_UPTIME_SECS:
        wait_secs = MIN_BOOT_UPTIME_SECS - uptime_secs
        _LOGGER.info(
            "Device uptime is %.1fs; waiting %.1fs past 20s ODPM window...",
            uptime_secs,
            wait_secs,
        )
        await control_flows.sleep_for_duration(timedelta(seconds=wait_secs))


def get_odpm_suspend_resume_counts(
    dut: android_device.AndroidDevice, moniker: str
) -> tuple[int, int]:
    """Returns `(suspend_count, resume_count)` from ODPM `root/power_management` Inspect."""
    escaped_moniker = moniker.replace(":", r"\:")
    selector = f"{escaped_moniker}:root/power_management"
    inspect_data_collection = dut.get_inspect_data(selectors=[selector])
    asserts.assert_greater(
        len(inspect_data_collection.data),
        0,
        f"Expected non-empty Inspect data for selector `{selector}`",
    )
    payload = inspect_data_collection.data[0].payload
    asserts.assert_is_not_none(
        payload,
        f"Expected valid Inspect payload for selector `{selector}`",
    )
    assert payload is not None
    power_management = payload["root"]["power_management"]
    return (
        int(power_management["suspend_count"]),
        int(power_management["resume_count"]),
    )


async def suspend_and_resume(
    dut: android_device.AndroidDevice, duration_secs: int
) -> None:
    """Suspends and resumes the DUT via USB power hub or `usb-cli cable-breaker`."""
    deadline = Deadline.from_timeout(_SUSPEND_RESUME_TIMEOUT)
    # Mock battery unplug before turning off the screen so Android's
    # PowerManagerService does not see a plugged->unplugged transition (which
    # would wake the display with WAKE_REASON_PLUGGED_IN) when USB VBUS drops.
    try:
        dut.adb.run(["shell", "dumpsys", "battery", "unplug"])
        dut.adb.run(["shell", "input", "keyevent", "KEYCODE_SLEEP"])
        await control_flows.sleep_for_duration(_SCREEN_OFF_SETTLE_DURATION)
        if dut.usb.power_hub is not None:
            dut.ffx.run(
                ["session", "drop-power-lease", "--allow-missing"],
                timeout=ODPM_CLI_TIMEOUT.total_seconds(),
                machine=ffx_types.MachineFormat.RAW,
            )
            await honeydew_power.suspend_resume(
                dut,
                deadline=deadline,
                base_idle_duration=timedelta(seconds=duration_secs),
            )
            return

        controller = usb_cli_controller.SystemPowerStateControllerUsingUsbCli(
            device_name=dut.device_name,
            ffx=dut.ffx,
            usb=dut.usb,
            device_logger=dut,
            inspect=dut,
            reboot_affordance=dut,
        )

        async def _attempt_usb_cli_suspend() -> None:
            try:
                await controller.idle_suspend_timer_based_resume(
                    duration=duration_secs,
                    verify_duration=False,
                )
            except spsc_interface.SystemPowerStateControllerError as err:
                if "Based on SAG inspect data" in str(err):
                    _LOGGER.warning(
                        "SAG did not suspend during idle (%s); retrying...",
                        err,
                    )
                    raise control_flows.RetriableError(str(err)) from err
                raise control_flows.RetryAbortingError(str(err)) from err

        await control_flows.retry_until_deadline(
            task=_attempt_usb_cli_suspend,
            deadline=deadline,
            retry_delay=timedelta(seconds=1),
        )
    finally:
        for cleanup_cmd in (
            ["shell", "dumpsys", "battery", "reset"],
            ["shell", "input", "keyevent", "KEYCODE_WAKEUP"],
        ):
            try:
                dut.adb.run(
                    cleanup_cmd,
                    timeout=ODPM_CLI_TIMEOUT.total_seconds(),
                    attempts=1,
                )
            except errors.HoneydewError as err:
                _LOGGER.warning(
                    "Failed to run post-suspend cleanup %s: %s",
                    cleanup_cmd,
                    err,
                )


def parse_display_state_residencies(
    output: str,
) -> dict[str, DisplayStateResidency]:
    """Parses `Display` state residency rows from `IPowerStats/default` dump."""
    residencies: dict[str, DisplayStateResidency] = {}
    for match in _DISPLAY_STATE_RESIDENCY_RE.finditer(output):
        state_name = match.group(1)
        residencies[state_name] = DisplayStateResidency(
            total_time_ms=int(match.group(2)),
            total_entry_count=int(match.group(3)),
            last_entry_timestamp_ms=int(match.group(4)),
        )
    return residencies


def read_display_state_residencies(
    dut: android_device.AndroidDevice,
) -> dict[str, DisplayStateResidency]:
    """Queries live `IPowerStats/default` HAL state residencies for `Display`.

    Caller must invoke this within `with dut.adb.use_adb_root():` so that
    `dumpsys android.hardware.power.stats.IPowerStats/default` is permitted.
    """
    output = dut.adb.run(["shell", "dumpsys", POWERSTATS_HAL_SERVICE])
    return parse_display_state_residencies(output)


def read_cpuidle_state(
    dut: android_device.AndroidDevice, state: str
) -> tuple[str, int, int]:
    """Reads `(name, residency_time_us, usage_count)` for a cpuidle state.

    Args:
        dut: Connected AndroidDevice DUT.
        state: Sysfs state subdirectory name (e.g., `state0` or `state1`).

    Returns:
        Tuple of `(state_name, residency_time_us, entry_usage_count)`.
    """
    name = dut.adb.run(["shell", "cat", f"{CPUIDLE_BASE}/{state}/name"]).strip()
    time_str = dut.adb.run(
        ["shell", "cat", f"{CPUIDLE_BASE}/{state}/time"]
    ).strip()
    usage_str = dut.adb.run(
        ["shell", "cat", f"{CPUIDLE_BASE}/{state}/usage"]
    ).strip()
    return name, int(time_str), int(usage_str)
