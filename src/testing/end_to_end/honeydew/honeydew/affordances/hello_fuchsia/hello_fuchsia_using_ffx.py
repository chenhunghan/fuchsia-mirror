# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""HelloFuchsia affordance implementation using FFX."""

from honeydew import errors
from honeydew.affordances.hello_fuchsia import errors as hello_fuchsia_errors
from honeydew.affordances.hello_fuchsia import hello_fuchsia
from honeydew.transports.ffx import errors as ffx_errors
from honeydew.transports.ffx import ffx as ffx_transport


class HelloFuchsiaUsingFfx(hello_fuchsia.HelloFuchsia):
    """HelloFuchsia affordance implementation using FFX."""

    def __init__(
        self,
        device_name: str,
        ffx: ffx_transport.FFX,
    ) -> None:
        self._device_name: str = device_name
        self._ffx: ffx_transport.FFX = ffx

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
            self._ffx.check_connection()
        except ffx_errors.FfxConnectionError as err:
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
            target_name: str = self._ffx.get_target_name()
        except errors.HoneydewError as err:
            raise hello_fuchsia_errors.HelloFuchsiaAffordanceError(
                f"Failed to get target name from {self._device_name}"
            ) from err

        if name and name.lower() == target_name.lower():
            raise hello_fuchsia_errors.HelloFuchsiaAffordanceError(
                f"Invalid value passed in name argument: {name}"
            )

        return (
            f"Hello Fuchsia, {name}!"
            if name
            else f"Hello Fuchsia, {target_name}!"
        )
