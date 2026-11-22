# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Abstract base class for controlling usb power hub hardware."""

import abc
import logging
from collections.abc import Mapping

from honeydew.auxiliary_devices import config as aux_config

_LOGGER: logging.Logger = logging.getLogger(__name__)


class UsbPowerHubError(Exception):
    """Exception class for UsbPowerHub relates error."""

    def __init__(self, msg: str | Exception) -> None:
        """Inits UsbPowerHubError with 'msg' (an error message string).

        Args:
            msg: an error message string or an Exception instance.

        Note: Additionally, logs 'msg' to debug log level file.
        """
        super().__init__(msg)
        _LOGGER.debug(repr(self), exc_info=True)


class UsbPowerHub(abc.ABC):
    """Abstract base class for usb power hub hardware."""

    @classmethod
    def from_config(
        cls,
        device_config: Mapping[str, object] | None = None,
    ) -> UsbPowerHub | None:
        """Returns the USB power hub configured in `device_config`, if any.

        Resolves `device_config["auxiliary_devices"]["usb_power_hub"]`
        (`hardware`, `implementation`) in the testbed config, when present.

        Args:
            device_config: Honeydew or Mobly controller config dictionary for
                the device.

        Returns:
            The configured `UsbPowerHub` instance, or `None` if none is
            configured.

        Raises:
            UsbPowerHubError: If the configuration is invalid, or the
                configured implementation cannot be loaded or is not a concrete
                subclass of `UsbPowerHub`.
        """
        return aux_config.create_device_from_config(
            device_config=device_config,
            device_key="usb_power_hub",
            expected_type=cls,
            error_cls=UsbPowerHubError,
        )

    # List all the public methods
    @abc.abstractmethod
    def power_off(self) -> None:
        """Turns off USB power to the device.

        Raises:
            UsbPowerHubError: In case of failure.
        """

    @abc.abstractmethod
    def power_on(self) -> None:
        """Turns on USB power to the device.

        Raises:
            UsbPowerHubError: In case of failure.
        """
