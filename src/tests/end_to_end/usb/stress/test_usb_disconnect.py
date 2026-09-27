# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""USB Disconnect stress tests (handles both physical and virtual)."""

import asyncio
import logging

import usb_lib
from honeydew import errors
from mobly import signals, test_runner

_LOGGER: logging.Logger = logging.getLogger(__name__)

_DEFAULT_RECONNECT_TIMEOUT_SEC: int = 60


class UsbDisconnectTest(usb_lib.UsbPowerHubBaseTest):
    """Mobly test for testing USB disconnects.

    Supports both physical disconnect (using hardware PDU/power hub) and virtual
    disconnect (using software authorization control).

    Required Mobly Test Params:
        num_iterations (int, optional): Number of times to execute the test.
            Defaults to 10 (or uses num_usb_disconnects if provided).
        num_usb_disconnects (int, optional): Alias for num_iterations.
        disconnect_duration_sec (int, optional): How long to stay disconnected.
            Defaults to 10.
        reconnect_timeout_sec (int, optional): How long to wait for the device
            to come back online after USB power is restored. Defaults to 60.
    """

    USB_POWER_HUB_REQUIRED: bool = True
    _reconnect_failed: bool = False

    async def pre_run(self) -> None:
        """Mobly method used to generate the test cases at run time."""
        test_arg_tuple_list: list[tuple[int]] = []

        num_iterations = int(
            self.user_params.get(
                "num_iterations",
                self.user_params.get("num_usb_disconnects", 10),
            )
        )
        for iteration in range(1, num_iterations + 1):
            test_arg_tuple_list.append((iteration,))

        self.generate_tests(
            test_logic=self._test_logic,
            name_func=self._name_func,
            arg_sets=test_arg_tuple_list,
        )

    async def setup_test(self) -> None:
        """setup_test is called once before running each test."""
        self._reconnect_failed = False
        await super().setup_test()

    async def teardown_test(self) -> None:
        """teardown_test is called once after running each test."""
        if self._reconnect_failed:
            _LOGGER.warning(
                "Skipping teardown health check because device failed to reconnect over USB."
            )
            return
        await super().teardown_test()

    async def _test_logic(self, iteration: int) -> None:
        """Test case logic that disconnects the USB from a fuchsia device."""
        _LOGGER.info(
            "Starting the Usb Disconnect test iteration# %s", iteration
        )

        disconnect_duration = int(
            self.user_params.get("disconnect_duration_sec", 10)
        )
        reconnect_timeout = int(
            self.user_params.get(
                "reconnect_timeout_sec", _DEFAULT_RECONNECT_TIMEOUT_SEC
            )
        )

        power_hub = self.require_usb_power_hub()

        try:
            await asyncio.wait_for(
                self.dut.wait_for_online(),
                timeout=reconnect_timeout,
            )
        except asyncio.TimeoutError as e:
            self._reconnect_failed = True
            raise signals.TestAbortAll(
                f"Device {self.dut.device_name} failed to come online before "
                f"starting iteration {iteration} within {reconnect_timeout}s."
            ) from e

        pre_disconnect_boot_id = await self.dut.boot_id()
        _LOGGER.info("Pre-disconnect Boot ID: %s", pre_disconnect_boot_id)
        self.dut.fuchsia_controller.before_usb_disconnect()
        try:
            self.dut.ffx.notify_intentional_disconnect()
            power_hub.power_off(port=self._usb_port)
            _LOGGER.info("Waiting for the device to go offline...")
            await asyncio.to_thread(self.dut.wait_for_offline)
            _LOGGER.info("Device is successfully offline.")

            if disconnect_duration > 0:
                _LOGGER.info("Sleeping for %d seconds...", disconnect_duration)
                await asyncio.sleep(disconnect_duration)
        finally:
            power_hub.power_on(port=self._usb_port)
            _LOGGER.info("Waiting for the device to go online...")
            try:
                await asyncio.wait_for(
                    self.dut.wait_for_online(),
                    timeout=reconnect_timeout,
                )
            except asyncio.TimeoutError as e:
                self._reconnect_failed = True
                raise signals.TestAbortAll(
                    f"Device {self.dut.device_name} failed to reconnect over USB "
                    f"network within {reconnect_timeout}s after USB power-on."
                ) from e
            self.dut.fuchsia_controller.after_usb_reconnect()
            post_reconnect_boot_id = await self.dut.boot_id()
            _LOGGER.info("Post-reconnect Boot ID: %s", post_reconnect_boot_id)
            if pre_disconnect_boot_id != post_reconnect_boot_id:
                raise errors.FuchsiaDeviceError(
                    f"Unexpected reboot detected during USB disconnect for {self.dut.device_name}. "
                    f"Boot ID before: {pre_disconnect_boot_id} != after: {post_reconnect_boot_id}"
                )
            _LOGGER.info("Device is successfully back online.")
            self.dut.health_check()

        _LOGGER.info(
            "Successfully ended the Usb Disconnect test iteration# %s",
            iteration,
        )

    def _name_func(self, iteration: int) -> str:
        """This function generates the names of each test case based on each
        argument set.

        The name function should have the same signature as the actual test
        logic function.

        Returns:
            Test case name
        """
        return f"test_usb_disconnect_{iteration}"


if __name__ == "__main__":
    test_runner.main()
