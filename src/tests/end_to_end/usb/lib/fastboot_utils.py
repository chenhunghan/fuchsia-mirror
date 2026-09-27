# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Shared Fastboot lifecycle utilities for Fuchsia USB end-to-end tests."""

import asyncio
import logging

import fidl_fuchsia_hardware_power_statecontrol as fhp_statecontrol
from honeydew import errors
from honeydew.auxiliary_devices.usb_power_hub import usb_power_hub
from honeydew.fuchsia_device import fuchsia_device
from honeydew.typing import custom_types as honeydew_types
from honeydew.utils import host_shell

_LOGGER: logging.Logger = logging.getLogger(__name__)


async def reboot_to_fastboot_mode(
    dut: fuchsia_device.FuchsiaDevice,
    usb_power_hub: usb_power_hub.UsbPowerHub | None = None,
    usb_port: int | None = None,
    timeout_sec: int = 30,
) -> None:
    """Reboots the DUT to bootloader/fastboot mode.

    Args:
        dut: Honeydew FuchsiaDevice instance.
        usb_power_hub: Optional USB Power Hub fixture.
        usb_port: Optional port number on the USB power hub.
        timeout_sec: Timeout in seconds applied per stage, not as a total budget.
            In the worst case (initial FIDL wait timeout, FFX fallback reboot,
            second wait timeout), this function can wait up to `2 * timeout_sec + 15`s.
            Callers should size GN test target timeout_secs accordingly.
    """
    # Ensure USB power is turned ON before rebooting so bootloader sees VBUS
    if usb_power_hub:
        usb_power_hub.power_on(port=usb_port)

    try:
        if await dut.fastboot.is_in_fastboot_mode():
            _LOGGER.info("Device is already in Fastboot mode.")
            dut.fastboot._ready = True
            return
    except Exception as e:
        _LOGGER.debug("Check is_in_fastboot_mode error: %s", e)

    _LOGGER.info("Rebooting device to bootloader via FIDL...")

    power_admin_endpoint = honeydew_types.FidlEndpoint(
        "/bootstrap/shutdown_shim",
        "fuchsia.hardware.power.statecontrol.Admin",
    )

    # TODO(b/527657910): Remove private _get_fastboot_node() call once Honeydew adds retry
    # polling inside _get_fastboot_node() when queried mid-reboot.
    try:
        await dut.fastboot._get_fastboot_node()
    except Exception as e:
        _LOGGER.debug("Pre-populating fastboot node ID raised exception: %s", e)

    try:
        dut.ffx.notify_intentional_disconnect()
        fc_transport = dut.fuchsia_controller
        power_proxy = fhp_statecontrol.AdminClient(
            fc_transport.connect_device_proxy(power_admin_endpoint)
        )
        await power_proxy.shutdown(
            options=fhp_statecontrol.ShutdownOptions(
                action=fhp_statecontrol.ShutdownAction.REBOOT_TO_BOOTLOADER,
                reasons=[fhp_statecontrol.ShutdownReason.DEVELOPER_REQUEST],
            )
        )
    except Exception as e:
        _LOGGER.debug(
            "Reboot command raised exception (expected if device rebooted quickly): %s",
            e,
        )

    _LOGGER.info("Waiting for device to enter fastboot mode...")
    try:
        await asyncio.wait_for(
            dut.fastboot.wait_for_fastboot_mode(),
            timeout=timeout_sec,
        )
    except asyncio.TimeoutError:
        _LOGGER.warning(
            "Device did not enter fastboot mode after FIDL shutdown. Attempting fallback via FFX reboot..."
        )
        try:
            dut.ffx.notify_intentional_disconnect()
            dut.ffx.run(
                cmd=["target", "reboot", "--bootloader"],
                log_status_on_failure=False,
                timeout=15,
            )
        except Exception as e:
            _LOGGER.debug("FFX reboot to bootloader exception: %s", e)
        await asyncio.wait_for(
            dut.fastboot.wait_for_fastboot_mode(),
            timeout=timeout_sec,
        )

    # Populate fastboot_node_id if needed and mark fastboot transport ready
    if dut.fastboot._fastboot_node_id is None:
        try:
            fb_devices_output = (
                host_shell.run(cmd=[dut.fastboot._fastboot_binary, "devices"])
                or ""
            )
            discovered_serials: list[str] = []
            for line in fb_devices_output.strip().split("\n"):
                tokens = line.split()
                if len(tokens) >= 2 and tokens[1] == "fastboot":
                    discovered_serials.append(tokens[0])

            if len(discovered_serials) > 1:
                _LOGGER.warning(
                    "Multiple devices detected in fastboot mode: %s; selecting %s",
                    discovered_serials,
                    discovered_serials[0],
                )

            if discovered_serials:
                dut.fastboot._fastboot_node_id = discovered_serials[0]
                _LOGGER.info(
                    "Discovered fastboot node ID from fastboot devices: %s",
                    dut.fastboot._fastboot_node_id,
                )
        except Exception as e:
            _LOGGER.debug(
                "Could not resolve fastboot serial from fastboot devices: %s",
                e,
            )

    dut.fastboot._ready = True


async def recover_to_fuchsia_mode(
    dut: fuchsia_device.FuchsiaDevice,
    usb_power_hub: usb_power_hub.UsbPowerHub | None = None,
    usb_port: int | None = None,
    timeout_sec: int = 60,
) -> None:
    """Ensures DUT is rebooted out of Fastboot and restored back to Fuchsia.

    Args:
        dut: Honeydew FuchsiaDevice instance.
        usb_power_hub: Optional USB Power Hub fixture.
        usb_port: Optional port number on the USB power hub.
        timeout_sec: Timeout in seconds applied per stage, not as a total budget.
            When rebooting from fastboot to Fuchsia, waiting for boot_to_fuchsia_mode
            and waiting for online each use timeout_sec (up to `2 * timeout_sec` total).
            Callers should size GN test target timeout_secs accordingly.

    Note on reconciliation:
        If the DUT was in Fastboot mode, it is rebooted to Fuchsia, waited for online up to
        timeout_sec, and on_device_boot() is invoked (raising FuchsiaDeviceError on timeout).
        If the DUT was not in Fastboot mode (e.g. already in Fuchsia or auto-rebooted), it is
        waited for online up to 15s and on_device_boot() is called to refresh device state;
        on failure, a warning is logged and health_check() is run as fallback.
    """
    _LOGGER.info("Ensuring device is booted back to Fuchsia...")
    if usb_power_hub:
        usb_power_hub.power_on(port=usb_port)

    was_in_fastboot = False
    try:
        if await dut.fastboot.is_in_fastboot_mode():
            was_in_fastboot = True
            _LOGGER.info(
                "Device is in Fastboot mode; issuing fastboot reboot to Fuchsia..."
            )
            await asyncio.wait_for(
                dut.fastboot.boot_to_fuchsia_mode(),
                timeout=timeout_sec,
            )
    except Exception as e:
        _LOGGER.warning("fastboot reboot to Fuchsia command error (%s).", e)

    if was_in_fastboot:
        _LOGGER.info("Waiting for device to come back online in Fuchsia...")
        try:
            await asyncio.wait_for(dut.wait_for_online(), timeout=timeout_sec)
        except asyncio.TimeoutError as e:
            raise errors.FuchsiaDeviceError(
                f"Timed out waiting for device to come online in Fuchsia after {timeout_sec}s"
            ) from e
        await dut.on_device_boot()
        _LOGGER.info("Device is successfully back online in Fuchsia.")
    else:
        try:
            await asyncio.wait_for(dut.wait_for_online(), timeout=15)
            await dut.on_device_boot()
            _LOGGER.info("Device is successfully back online in Fuchsia.")
        except Exception as e:
            _LOGGER.warning(
                "Device failed to come online or complete boot handling (%s); falling back to health check.",
                e,
            )
            dut.health_check()
