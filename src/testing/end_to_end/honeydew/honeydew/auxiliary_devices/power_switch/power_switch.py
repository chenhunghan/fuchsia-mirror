# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Abstract base class for power switch hardware."""

import abc
import logging
from collections.abc import Mapping

from honeydew.auxiliary_devices import config as aux_config

_LOGGER: logging.Logger = logging.getLogger(__name__)


class PowerSwitchError(Exception):
    """Exception class for PowerSwitch relates error."""

    def __init__(self, msg: str | Exception) -> None:
        """Inits PowerSwitchError with 'msg' (an error message string).

        Args:
            msg: an error message string or an Exception instance.

        Note: Additionally, logs 'msg' to debug log level file.
        """
        super().__init__(msg)
        _LOGGER.debug(repr(self), exc_info=True)


class PowerSwitch(abc.ABC):
    """Abstract base class for power switch hardware."""

    @classmethod
    def from_config(
        cls,
        device_config: Mapping[str, object] | None = None,
    ) -> PowerSwitch | None:
        """Returns the power switch configured in `device_config`, if any.

        Resolves `device_config["auxiliary_devices"]["power_switch"]`
        (`hardware`, `implementation`) in the testbed config, when present.

        Args:
            device_config: Honeydew or Mobly controller config dictionary for
                the device.

        Returns:
            The configured `PowerSwitch` instance, or `None` if none is
            configured.

        Raises:
            PowerSwitchError: If the configuration is invalid, or the
                configured implementation cannot be loaded or is not a concrete
                subclass of `PowerSwitch`.
        """
        return aux_config.create_device_from_config(
            device_config=device_config,
            device_key="power_switch",
            expected_type=cls,
            error_cls=PowerSwitchError,
        )

    # List all the public methods
    @abc.abstractmethod
    def power_off(self) -> None:
        """Turns off power to the device.

        Raises:
            PowerSwitchError: In case of failure.
        """

    @abc.abstractmethod
    def power_on(self) -> None:
        """Turns on power to the device.

        Raises:
            PowerSwitchError: In case of failure.
        """
