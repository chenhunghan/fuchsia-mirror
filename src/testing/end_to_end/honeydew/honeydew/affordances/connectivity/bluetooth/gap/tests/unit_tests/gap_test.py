# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
# pylint: disable=protected-access
"""Unit tests for gap.py"""

import unittest
from unittest import mock

from honeydew import affordances_capable
from honeydew.affordances.connectivity.bluetooth.gap import gap
from honeydew.transports.fuchsia_controller import (
    fuchsia_controller as fc_transport,
)


class BluetoothGapAsyncTests(unittest.IsolatedAsyncioTestCase):
    """Unittests for Bluetooth GAP over FC (Async)."""

    async def asyncSetUp(self) -> None:
        await super().asyncSetUp()
        self.reboot_affordance_obj = mock.MagicMock(
            spec=affordances_capable.RebootCapableDevice
        )
        self.fc_transport_obj = mock.MagicMock(
            spec=fc_transport.FuchsiaController
        )

        self.bluetooth_gap_obj = gap.Gap(
            device_name="fuchsia-emulator",
            fuchsia_controller=self.fc_transport_obj,
            reboot_affordance=self.reboot_affordance_obj,
        )

    def test_gap(self) -> None:
        """Test case for gap.py"""
        self.assertIsInstance(self.bluetooth_gap_obj, gap.Gap)


if __name__ == "__main__":
    unittest.main()
