# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""AndroidDevice device class implementation."""

import logging

from honeydew import errors
from honeydew.fuchsia_device import fuchsia_device
from honeydew.transports.adb import adb as adb_transport
from honeydew.utils import properties

_LOGGER: logging.Logger = logging.getLogger(__name__)


class AndroidDevice(fuchsia_device.FuchsiaDevice):
    """AndroidDevice device class implementation."""

    @properties.Transport
    def adb(self) -> adb_transport.Adb:
        """Returns the ADB transport object.

        Returns:
            ADB transport interface implementation.

        Raises:
            errors.NotEnabledError: If ADB transport is not enabled.
            errors.NotSupportedError: If ADB transport is not supported by Fuchsia device.
        """
        run_isolated_server = True
        vendor_keys_path = None
        enabled = True

        if self._config:
            adb_config = self._config.get("transports", {}).get("adb", {})
            run_isolated_server = adb_config.get("run_isolated_server", True)
            vendor_keys_path = adb_config.get("vendor_keys_path")
            enabled = adb_config.get("enabled", True)

        # Note - An existing ADB implementation in //vendor/google is used by
        # some Lacewing tests. Running two ADB server implementations against
        # the same device causes conflicts. Tests that manage ADB externally
        # can set `enabled` to False to disable Honeydew's ADB transport.
        # TODO(b/559563915): Delete this `enabled` config once legacy ADB
        # server implementations are removed and all Lacewing tests use
        # Honeydew's ADB transport.
        if not enabled:
            _LOGGER.warning(
                "User has requested to not enable the ADB transport for '%s'",
                self.device_name,
            )
            raise errors.NotEnabledError(
                f"User has requested to not enable the ADB transport for '{self.device_name}'"
            )

        serial_number: str | None = self._device_info.serial_number
        if serial_number is None:
            try:
                serial_number = self.ffx.serial_number
            except Exception as err:
                _LOGGER.debug(
                    "Failed to get serial number from FFX for %s: %s",
                    self.device_name,
                    err,
                )
                serial_number = None

        if serial_number is None:
            _LOGGER.warning(
                "ADB transport is not supported on %s as 'serial_number' was not provided and could not be retrieved via FFX",
                self.device_name,
            )
            raise errors.NotSupportedError(
                f"ADB transport is not supported on {self.device_name} "
                f"as 'serial_number' was not provided and could not be retrieved via FFX"
            )

        adb_obj: adb_transport.Adb = adb_transport.Adb(
            device_name=self.device_name,
            serial_number=serial_number,
            run_isolated_server=run_isolated_server,
            vendor_keys_path=vendor_keys_path,
            ffx_transport=self.ffx,
        )
        self.register_for_on_device_close(adb_obj.close)
        return adb_obj

    def _check_connections(self) -> None:
        """Checks health of all device transports including ADB."""
        super()._check_connections()

        try:
            self.adb.check_connection()
        except errors.NotEnabledError:
            _LOGGER.info(
                "ADB is not enabled on %s, so skipping the ADB connection check.",
                self.device_name,
            )

    async def on_device_boot(self) -> None:
        """Take actions after the device is rebooted.

        Raises:
            FuchsiaControllerError: On communications failure.
            Sl4fError: On communications failure.
        """
        try:
            self.adb.on_device_boot()
        except errors.NotEnabledError:
            _LOGGER.info(
                "ADB is not enabled on %s, so skipping ADB on_device_boot.",
                self.device_name,
            )

        await super().on_device_boot()
