# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""HelloFuchsia affordance implementation using Fuchsia-Controller."""

import fidl_fuchsia_developer_remotecontrol as fd_remotecontrol
from honeydew import errors
from honeydew.affordances.hello_fuchsia import errors as hello_fuchsia_errors
from honeydew.affordances.hello_fuchsia import hello_fuchsia
from honeydew.transports.fuchsia_controller import errors as fc_errors
from honeydew.transports.fuchsia_controller import (
    fuchsia_controller as fc_transport,
)
from honeydew.typing import custom_types

_REMOTE_CONTROL_PROXIES = custom_types.FidlEndpoint(
    "/core/remote-control", "fuchsia.developer.remotecontrol.RemoteControl"
)


class HelloFuchsiaUsingFc(hello_fuchsia.HelloFuchsia):
    """HelloFuchsia affordance implementation using Fuchsia-Controller."""

    def __init__(
        self,
        device_name: str,
        fuchsia_controller: fc_transport.FuchsiaController,
    ) -> None:
        self._device_name: str = device_name
        self._fc_transport: fc_transport.FuchsiaController = fuchsia_controller

        self.verify_supported()

    def verify_supported(self) -> None:
        """Verifies that HelloFuchsia affordance is supported by the Fuchsia device.

        This method should be called in every affordance implementation's `__init__()` so that if an
        affordance is used on a Fuchsia device that does not support it, it will raise
        NotSupportedError.

        Raises:
            NotSupportedError: If affordance is not supported.
        """
        try:
            self._fc_transport.check_connection()
        except fc_errors.FuchsiaControllerConnectionError as err:
            raise errors.NotSupportedError(
                f"HelloFuchsia affordance is not supported by {self._device_name}"
            ) from err

    async def greeting(self, name: str | None = None) -> str:
        """Returns a string that contains greeting's message.

        Args:
            name: Optional name to greet. Defaults to the target device name.

        Returns:
            greeting's message str

        Raises:
            HelloFuchsiaAffordanceError: Failed to construct greeting's message.
        """
        try:
            rcs_proxy = fd_remotecontrol.RemoteControlClient(
                self._fc_transport.connect_device_proxy(_REMOTE_CONTROL_PROXIES)
            )
            resp = (await rcs_proxy.identify_host()).unwrap()
            target_name: str | None = resp.response.nodename
        except errors.FIDL_ERRORS + (
            RuntimeError,
            AssertionError,
            errors.HoneydewError,
        ) as err:
            raise hello_fuchsia_errors.HelloFuchsiaAffordanceError(
                f"Failed to get nodename from {self._device_name}"
            ) from err

        if target_name is None:
            raise hello_fuchsia_errors.HelloFuchsiaAffordanceError(
                f"Nodename not populated in IdentifyHost response for {self._device_name}"
            )

        if name and name.lower() == target_name.lower():
            raise hello_fuchsia_errors.HelloFuchsiaAffordanceError(
                f"Invalid value passed in name argument: {name}"
            )

        return (
            f"Hello Fuchsia, {name}!"
            if name
            else f"Hello Fuchsia, {target_name}!"
        )
