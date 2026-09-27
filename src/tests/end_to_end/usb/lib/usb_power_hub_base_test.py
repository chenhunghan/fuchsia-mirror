# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Base test class for Mobly tests requiring or optionally using a USB power hub fixture."""

import logging

import fuchsia_base_test
from honeydew import errors
from honeydew.auxiliary_devices.usb_power_hub import usb_power_hub
from mobly import signals

_LOGGER: logging.Logger = logging.getLogger(__name__)


class UsbPowerHubBaseTest(fuchsia_base_test.FuchsiaBaseTest):
    """Base test class for Mobly tests that bind to a USB power hub fixture.

    Attributes:
        USB_POWER_HUB_REQUIRED: If True, failure to acquire the USB power hub fixture
            or power on the port will abort the test class (signals.TestAbortClass).
            If False, failures are logged as warnings and the test proceeds with
            both _usb_power_hub and _usb_port set to None.
    """

    USB_POWER_HUB_REQUIRED: bool = True

    _usb_power_hub: usb_power_hub.UsbPowerHub | None = None
    _usb_port: int | None = None

    def require_usb_power_hub(self) -> usb_power_hub.UsbPowerHub:
        """Returns the bound hub, raising if it was not acquired."""
        if self._usb_power_hub is None:
            raise errors.HoneydewError(
                "USB power hub fixture is required but was not acquired."
            )
        return self._usb_power_hub

    async def setup_class(self) -> None:
        """setup_class is called once before running tests."""
        await super().setup_class()
        hub: usb_power_hub.UsbPowerHub | None = self.dut.usb_power_hub
        port: int | None = self.dut.usb_power_hub_port
        if hub is None:
            if self.USB_POWER_HUB_REQUIRED:
                raise signals.TestAbortClass(
                    f"USB power hub is not configured for {self.dut.device_name}."
                )
            _LOGGER.info(
                "No USB power hub configured for %s. Proceeding without one.",
                self.dut.device_name,
            )
            self._usb_power_hub = None
            self._usb_port = None
        else:
            self._usb_power_hub = hub
            self._usb_port = port
            self._usb_power_hub.power_on(port=self._usb_port)
            _LOGGER.info(
                "Successfully bound USB Power Hub fixture on port %s",
                self._usb_port,
            )

        # Pre-cache PersistentProperty values (board, product) while the device is online
        # in Fuchsia OS. Because they are lazily evaluated, if left un-evaluated and the test
        # later fails while in Fastboot mode or offline, Mobly's post-test metadata collection
        # (test_summary.yaml) would trigger an FFX query against the offline device.
        _ = self.dut.board
        _ = self.dut.product
