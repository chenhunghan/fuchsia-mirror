# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Provides Host-(Fuchsia)Target interactions via Fuchsia-Controller."""

import logging

import fuchsia_controller_py as fuchsia_controller
from honeydew import errors
from honeydew.affordances_capable import FuchsiaDeviceAddressChange
from honeydew.transports import transport
from honeydew.transports.ffx import config as ffx_config
from honeydew.transports.fuchsia_controller import errors as fc_errors
from honeydew.typing import custom_types
from honeydew.utils import decorators

_LOGGER: logging.Logger = logging.getLogger(__name__)

# Exceptions raised by the native Fuchsia-Controller bindings on failure. Most
# failures raise `FcTransportStatus`, but internal failures (e.g. target not
# ready in `Context.target_wait()`) raise `RuntimeError`. See:
# - `mod::set_python_exception()` in
#   //src/developer/fuchsia-controller/cpp/fuchsia_controller_internal/mod.cc,
#   which maps `FC_ERR_INTERNAL` to `RuntimeError` and most other statuses to
#   `FcTransportStatus`.
# - `LibraryCommand::TargetWait` in
#   //src/developer/fuchsia-controller/src/commands.rs, which returns
#   `FC_ERR_INTERNAL` when the target wait fails.
_CONTROLLER_ERRORS: tuple[type[Exception], ...] = errors.FIDL_ERRORS + (
    RuntimeError,
    errors.HoneydewError,
)


class FuchsiaController(transport.Transport):
    """Provides Host-(Fuchsia)Target interactions via Fuchsia-Controller.

    Args:
        target_name: Fuchsia device name.
        ffx_config_data: Configuration associated with FuchsiaController, FFX.
        target_addr: Fuchsia device target address (IP or USB).
        device_address_change: Object that implements FuchsiaDeviceAddressChange to handle Fuchsia
            device address changes.

    Raises:
        ValueError: If target_addr is provided and device_address_change is not provided.
        FuchsiaControllerConnectionError: If target is not ready.
        FuchsiaControllerError: Failed to instantiate.
    """

    def __init__(
        self,
        target_name: str,
        ffx_config_data: ffx_config.FfxConfigData,
        target_addr: custom_types.TargetAddr | None = None,
        device_address_change: FuchsiaDeviceAddressChange | None = None,
    ) -> None:
        self._target_name: str = target_name

        self._ffx_config_data: ffx_config.FfxConfigData = ffx_config_data

        self._target_addr: custom_types.TargetAddr | None = target_addr
        if self._target_addr is not None and device_address_change is None:
            raise ValueError(
                "Pass 'device_address_change' argument also when 'target_addr' is passed"
            )
        self._device_address_change: FuchsiaDeviceAddressChange | None = (
            device_address_change
        )
        if self._device_address_change:
            self._device_address_change.register_for_on_device_address_change(
                fn=self._on_device_address_change
            )

        self._target: str
        if self._target_addr:
            self._target = str(self._target_addr)
        else:
            self._target = self._target_name

        self.ctx: fuchsia_controller.Context

        self.verify_supported()
        self.create_context()
        self.check_connection()

    def verify_supported(self) -> None:
        """Verifies that Fuchsia-Controller transport is supported by the Fuchsia device."""
        # All Fuchsia devices support Fuchsia-Controller transport.

    def create_context(self) -> None:
        """Creates the fuchsia-controller context in ffx strict mode.

        Raises:
            FuchsiaControllerError: Failed to create FuchsiaController Context.
            FuchsiaControllerConnectionError: If target is not ready.
        """
        try:
            config = self._ffx_config_data.get_config_dict()
            _LOGGER.debug(
                "Creating strict Fuchsia-Controller Context with "
                "target='%s', config='%s'",
                self._target,
                config,
            )
            self.ctx = fuchsia_controller.Context.strict(
                config=config, target=self._target
            )
        except _CONTROLLER_ERRORS as err:
            raise fc_errors.FuchsiaControllerError(
                "Failed to create Fuchsia-Controller context"
            ) from err

    @decorators.liveness_check
    def check_connection(self, timeout: int = 0) -> None:
        """Checks the Fuchsia-Controller connection from host to Fuchsia device.

        Args:
            timeout: Maximum time in seconds to wait for the connection.
                Defaults to 0 (waits indefinitely).

        Raises:
            FuchsiaControllerConnectionError
        """
        try:
            _LOGGER.info(
                "Waiting for Fuchsia-Controller to check the "
                "connection from host to %s...",
                self._target_name,
            )
            self.ctx.target_wait(timeout=timeout)
            _LOGGER.info(
                "Fuchsia-Controller completed the connection check from host "
                "to %s...",
                self._target_name,
            )
        except _CONTROLLER_ERRORS as err:
            raise fc_errors.FuchsiaControllerConnectionError(
                f"Fuchsia-Controller connection check failed for "
                f"{self._target_name} with error: {err}"
            )

    def connect_device_proxy(
        self, fidl_end_point: custom_types.FidlEndpoint
    ) -> fuchsia_controller.Channel:
        """Opens a proxy to the specified FIDL end point.

        Args:
            fidl_end_point: FIDL end point tuple containing moniker and protocol
              name.

        Raises:
            FuchsiaControllerError: On FIDL communication failure.

        Returns:
            FIDL channel to proxy.
        """
        try:
            return self.ctx.connect_device_proxy(
                fidl_end_point.moniker, fidl_end_point.protocol
            )
        except fuchsia_controller.FcTransportStatus as status:
            raise fc_errors.FuchsiaControllerError(
                "Fuchsia Controller FIDL Error"
            ) from status

    def _on_device_address_change(
        self, target_addr: custom_types.TargetAddr
    ) -> None:
        """Callback method that gets invoked when device address changes.

        Args:
            target_addr: New target address of the device.
        """
        self._target_addr = target_addr
        self._target = str(self._target_addr)

        self.create_context()
        self.check_connection()

    def channel_create(
        self,
    ) -> tuple[fuchsia_controller.Channel, fuchsia_controller.Channel]:
        """Opens a pair of connected channels, usef for FIDL endpoints.

        Raises: FuchsiaControllerError: On failure to create the channels.
        """
        try:
            return self.ctx.channel_create()
        except fuchsia_controller.FcTransportStatus as status:
            raise fc_errors.FuchsiaControllerError(
                "Fuchsia Controller Channel Create Error"
            ) from status

    def socket_create(
        self,
    ) -> tuple[fuchsia_controller.Socket, fuchsia_controller.Socket]:
        """Opens a pair of connected sockets, used for FIDL endpoints.

        Raises: FuchsiaControllerError: On failure to create the sockets.
        """
        try:
            return self.ctx.socket_create()
        except fuchsia_controller.FcTransportStatus as status:
            raise fc_errors.FuchsiaControllerError(
                "Fuchsia Controller Socket Create Error"
            ) from status
