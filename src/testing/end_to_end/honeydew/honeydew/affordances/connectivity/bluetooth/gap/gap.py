# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Bluetooth Gap affordance implementation using Fuchsia Controller."""

from honeydew import affordances_capable
from honeydew.affordances.connectivity.bluetooth.bluetooth_common import (
    bluetooth_common,
)
from honeydew.transports.fuchsia_controller import (
    fuchsia_controller as fc_transport,
)


class Gap(bluetooth_common.BluetoothCommon):
    """Async BluetoothGap Common affordance implementation using Fuchsia Controller.

    Args:
        device_name: Device name returned by `ffx target list`.
        fuchsia_controller: Fuchsia Controller transport.
        reboot_affordance: Object that implements RebootCapableDevice.
    """

    def __init__(
        self,
        device_name: str,
        fuchsia_controller: fc_transport.FuchsiaController,
        reboot_affordance: affordances_capable.RebootCapableDevice,
    ) -> None:
        super().__init__(device_name, fuchsia_controller, reboot_affordance)

    def verify_supported(self) -> None:
        """Check if Bluetooth gap is supported on the DUT.
        Raises:
            NotSupportedError: Bluetooth Gap affordance is not supported by Fuchsia device.
        """
        super().verify_supported()
        # TODO(http://b/409623783): Implement the method logic
