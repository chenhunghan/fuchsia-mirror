# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""DMC based virtual UsbPowerHub auxiliary device implementation."""

import logging
import os
import platform

from honeydew import errors
from honeydew.auxiliary_devices.usb_power_hub import (
    usb_power_hub,
    usb_power_hub_using_dmc,
)
from honeydew.utils import host_shell

from . import usb_reset

_LOGGER: logging.Logger = logging.getLogger(__name__)


class _DmcVirtualUsbPowerHubBase(usb_power_hub.UsbPowerHub):
    """Shared base of the DMC based virtual USB power hubs.

    Runs `dmc` commands that make the device management server (DMS) on the
    host the device is attached to act on the device's USB port. Only works
    for devices hosted in Fuchsia infra labs, where the `DMC_PATH`
    environment variable is set.

    Args:
        device_name: Device name returned by `ffx target list`, which is also
            the device nodename known to DMS.

    Raises:
        usb_power_hub.UsbPowerHubError: If `DMC_PATH` is not set.
    """

    def __init__(self, device_name: str) -> None:
        super().__init__()
        self._name: str = device_name

        try:
            self._dmc_path: str = os.environ[
                usb_power_hub_using_dmc.DMC_PATH_KEY
            ]
        except KeyError as err:
            raise usb_power_hub.UsbPowerHubError(
                f"{usb_power_hub_using_dmc.DMC_PATH_KEY} environment variable "
                f"is not set on {platform.node()}"
            ) from err

    def _run_dmc(self, subcommand: str, *args: str) -> None:
        """Runs a `dmc` subcommand for the device.

        Args:
            subcommand: The `dmc` subcommand to run.
            *args: Extra arguments after `-nodename <device_name>`.

        Raises:
            usb_power_hub.UsbPowerHubError: If the command fails, e.g. the DMS
                on the host does not support it yet.
        """
        cmd: list[str] = [
            self._dmc_path,
            subcommand,
            "-nodename",
            self._name,
            *args,
        ]
        try:
            host_shell.run(cmd=cmd)
        except errors.HostCmdError as err:
            raise usb_power_hub.UsbPowerHubError(err) from err


class DmcVirtualUsbPowerHub(_DmcVirtualUsbPowerHubBase):
    """Virtually unplugs and plugs in the device through DMC in Fuchsia infra.

    The infra counterpart of `LinuxVirtualUsbAuthorizePowerHub`. Instead of
    writing the host sysfs `authorized` attribute itself, it runs
    `dmc set-usb-authorized`, which makes DMS on the host look up the device's
    USB bus ID and toggle its `authorized` attribute. DMS also authorizes the
    device again in its task cleanup, health check and recovery, so a test
    that dies while the device is unplugged does not leave it disconnected.

    Args:
        device_name: See `_DmcVirtualUsbPowerHubBase`.

    Raises:
        usb_power_hub.UsbPowerHubError: If `DMC_PATH` is not set.
    """

    def power_off(self, port: int | None = None) -> None:
        """Deauthorizes (virtually unplugs) the USB device.

        Args:
            port: None. Not used by this implementation.

        Raises:
            usb_power_hub.UsbPowerHubError: In case of failure.
        """
        _LOGGER.info("Virtually unplugging USB device of %s...", self._name)
        self._set_usb_authorized("off")
        _LOGGER.info(
            "Successfully virtually unplugged USB device of %s.", self._name
        )

    def power_on(self, port: int | None = None) -> None:
        """Authorizes (virtually plugs in) the USB device.

        Args:
            port: None. Not used by this implementation.

        Raises:
            usb_power_hub.UsbPowerHubError: In case of failure.
        """
        _LOGGER.info("Virtually plugging in USB device of %s...", self._name)
        self._set_usb_authorized("on")
        _LOGGER.info(
            "Successfully virtually plugged in USB device of %s.", self._name
        )

    def _set_usb_authorized(self, state: str) -> None:
        """Runs `dmc set-usb-authorized` for the device.

        Args:
            state: "on" or "off".

        Raises:
            usb_power_hub.UsbPowerHubError: If the command fails.
        """
        self._run_dmc("set-usb-authorized", "-state", state)


class DmcVirtualUsbResetPowerHub(
    _DmcVirtualUsbPowerHubBase, usb_reset.UsbResetCapable
):
    """Resets the device's USB link through DMC in Fuchsia infra.

    The infra counterpart of `LinuxVirtualUsbResetPowerHub`. Instead of
    issuing the USBDEVFS_RESET ioctl itself, `reset()` runs
    `dmc reset-usb-device`, which makes DMS on the host issue it on the
    device's usbfs node. The host drives a bus reset on the port and
    re-enumerates the device without cutting VBUS, so there is no "off"
    phase: `power_off()` is not supported, and `power_on()` is a no-op since
    this hub never leaves the link down.

    Args:
        device_name: See `_DmcVirtualUsbPowerHubBase`.

    Raises:
        usb_power_hub.UsbPowerHubError: If `DMC_PATH` is not set.
    """

    def reset(self, port: int | None = None) -> None:
        """Resets the USB link of the device.

        Args:
            port: None. Not used by this implementation.

        Raises:
            usb_power_hub.UsbPowerHubError: In case of failure.
        """
        _LOGGER.info("Resetting USB device of %s...", self._name)
        self._run_dmc("reset-usb-device")
        _LOGGER.info("Successfully reset USB device of %s.", self._name)

    def power_off(self, port: int | None = None) -> None:
        """Not supported, as a USB reset has no "off" phase.

        Args:
            port: None. Not used by this implementation.

        Raises:
            usb_power_hub.UsbPowerHubError: Always.
        """
        raise usb_power_hub.UsbPowerHubError(
            f"{type(self).__name__} cannot power off the USB link; use reset()."
        )

    def power_on(self, port: int | None = None) -> None:
        """No-op, as this hub never leaves the USB link down.

        Args:
            port: None. Not used by this implementation.
        """
        _LOGGER.debug(
            "USB device of %s is never left disconnected by %s; nothing to do.",
            self._name,
            type(self).__name__,
        )
