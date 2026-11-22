#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Device-agnostic helper utilities for battery End-to-End tests."""

import json
import logging
from collections.abc import Mapping

import fidl_fuchsia_hardware_power_charger as f_charger
from honeydew import errors as honeydew_errors
from honeydew.affordances.drivers.battery_manager.utils import (
    types as battery_types,
)
from honeydew.fuchsia_device import fuchsia_device
from honeydew.transports.ffx import types as ffx_types
from honeydew.typing.custom_types import FidlEndpoint
from honeydew.utils import control_flows
from mobly import asserts

_LOGGER: logging.Logger = logging.getLogger(__name__)

BATTERY_SERVICE_CAPABILITY: str = "fuchsia.hardware.power.battery.Service"
CHARGER_DEBUG_SERVICE_CAPABILITY: str = (
    "fuchsia.hardware.power.charger.DebugService"
)
_CHARGER_DEBUG_ENDPOINT_PATH: str = (
    f"{CHARGER_DEBUG_SERVICE_CAPABILITY}/default/debug"
)


def parse_bool_param(
    user_params: Mapping[str, object], key: str, *, default: bool = False
) -> bool:
    """Parses a boolean Mobly user parameter safely across YAML and CLI types."""
    if key not in user_params:
        return default
    val = user_params[key]
    if isinstance(val, bool):
        return val
    return str(val).strip().lower() in ("true", "1")


def try_discover_service_moniker(
    dut: fuchsia_device.FuchsiaDevice, service_capability: str
) -> str | None:
    """Finds the component moniker declaring or exposing `service_capability`."""
    output = dut.ffx.run(
        ["component", "capability", service_capability],
        machine=ffx_types.MachineFormat.JSON,
    )
    entries = json.loads(output)
    if isinstance(entries, list):
        for entry in entries:
            if not isinstance(entry, dict):
                continue
            if entry.get("capability") != service_capability:
                continue
            entry_type = entry.get("type")
            is_provider = entry_type == "declare_by" or (
                entry_type == "expose_by"
                and entry.get("source") == "self"
                and entry.get("target") == "parent"
            )
            moniker = entry.get("moniker")
            if (
                is_provider
                and isinstance(moniker, str)
                and moniker
                and "devfs_driver" not in moniker
            ):
                return moniker
    return None


def discover_service_moniker(
    dut: fuchsia_device.FuchsiaDevice, service_capability: str
) -> str:
    """Discovers the component moniker providing `service_capability` or fails."""
    moniker = try_discover_service_moniker(dut, service_capability)
    if moniker is None:
        asserts.fail(
            f"No component declaring `{service_capability}` found on "
            f"{dut.device_name}"
        )
    assert moniker is not None
    return moniker


def discover_battery_moniker(dut: fuchsia_device.FuchsiaDevice) -> str:
    """Discovers the bound battery driver moniker by FIDL service capability."""
    return discover_service_moniker(dut, BATTERY_SERVICE_CAPABILITY)


def resolve_driver_url(dut: fuchsia_device.FuchsiaDevice, moniker: str) -> str:
    """Resolves the component URL for a driver moniker via `ffx component show`."""
    show_output = dut.ffx.run(
        ["component", "show", moniker],
        machine=ffx_types.MachineFormat.RAW,
    )
    for raw_line in show_output.splitlines():
        line = raw_line.strip()
        if line.startswith("URL:"):
            url = line.split("URL:", 1)[1].strip()
            if url:
                return url
    asserts.fail(
        f"Could not resolve component URL for moniker `{moniker}` in:\n"
        f"{show_output}"
    )
    return ""


async def poll_rebound_fidl(
    dut: fuchsia_device.FuchsiaDevice,
    *,
    expect_cycle_count: bool = False,
) -> tuple[battery_types.BatterySpec, battery_types.BatteryStatus]:
    """Reconnects to the battery FIDL service and queries spec and status.

    Raises:
        control_flows.RetriableError: If the battery driver is still rebinding
            after restart.
    """
    try:
        dut.fuchsia_controller.create_context()
        await dut.battery.make_ready()
        spec = await dut.battery.get_spec()
        status = await dut.battery.get_status()
    except (honeydew_errors.HoneydewError, RuntimeError) as err:
        raise control_flows.RetriableError(
            f"Battery FIDL service not yet ready after restart: {err}"
        ) from err
    if (
        (expect_cycle_count and status.cycle_count is None)
        or status.full_charge_capacity_uah is None
        or status.remaining_capacity_uah is None
    ):
        raise control_flows.RetriableError(
            f"Battery status incomplete after restart: {status}"
        )
    return spec, status


def _connect_charger_debug(
    dut: fuchsia_device.FuchsiaDevice,
) -> f_charger.DebugClient:
    """Connects a FIDL `DebugClient` to `fuchsia.hardware.power.charger.DebugService`."""
    moniker = discover_service_moniker(dut, CHARGER_DEBUG_SERVICE_CAPABILITY)
    endpoint = FidlEndpoint(moniker, _CHARGER_DEBUG_ENDPOINT_PATH)
    channel = dut.fuchsia_controller.connect_device_proxy(endpoint)
    return f_charger.DebugClient(channel)


async def set_charger_discharging(dut: fuchsia_device.FuchsiaDevice) -> None:
    """Applies a sticky `OperatingMode.DISCHARGING` override via `DebugService`."""
    client = _connect_charger_debug(dut)
    res = await client.set_control(
        options=f_charger.ControlOptions(
            operating_mode=f_charger.OperatingMode.DISCHARGING
        )
    )
    if res.err is not None:
        asserts.fail(
            f"Charger Debug.SetControl(DISCHARGING) failed with error: {res.err}"
        )


async def clear_charger_debug_control(
    dut: fuchsia_device.FuchsiaDevice, *, check: bool = True
) -> None:
    """Withdraws all sticky charger `DebugService` overrides."""
    moniker = try_discover_service_moniker(
        dut, CHARGER_DEBUG_SERVICE_CAPABILITY
    )
    if moniker is None:
        if check:
            asserts.fail(
                f"No component declaring `{CHARGER_DEBUG_SERVICE_CAPABILITY}` "
                f"found on {dut.device_name}"
            )
        return
    try:
        endpoint = FidlEndpoint(moniker, _CHARGER_DEBUG_ENDPOINT_PATH)
        channel = dut.fuchsia_controller.connect_device_proxy(endpoint)
        client = f_charger.DebugClient(channel)
        res = await client.clear_control(options=f_charger.ControlOptions())
        if res.err is not None:
            msg = f"Charger Debug.ClearControl() returned error: {res.err}"
            if check:
                asserts.fail(msg)
            _LOGGER.warning(msg)
    except Exception as err:  # pylint: disable=broad-except
        if check:
            raise
        _LOGGER.warning(
            "Best-effort ClearControl failed on %s: %s",
            dut.device_name,
            err,
        )
