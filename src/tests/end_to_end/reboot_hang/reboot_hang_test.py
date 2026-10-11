# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import asyncio
import json
import logging

import fuchsia_base_test
from mobly import asserts, test_runner

_LOGGER: logging.Logger = logging.getLogger(__name__)


class RebootHangTest(fuchsia_base_test.FuchsiaBaseTest):
    async def setup_test(self) -> None:
        await super().setup_test()
        self.device = self.fuchsia_devices[0]

    async def test_reboot_to_bootloader_on_hang(self) -> None:
        # 0. Register the test driver (since it is ephemeral).
        _LOGGER.info("Registering hang-on-stop driver...")
        self.device.ffx.run(
            [
                "driver",
                "register",
                "fuchsia-pkg://fuchsia.com/hang-on-stop#meta/hang-on-stop.cm",
            ]
        )

        # 1. Dynamically create the virtual parent device to bind the hang driver.
        _LOGGER.info("Adding test node to trigger hang driver...")
        self.device.ffx.run(
            [
                "driver",
                "test-node",
                "add",
                "hang_parent",
                "fuchsia.test.TEST_CHILD=hang_parent",
            ]
        )

        # Verify driver bound
        _LOGGER.info("Verifying hang-on-stop driver is bound...")

        bound = False
        for i in range(10):
            try:
                stdout = self.device.ffx.run(
                    ["driver", "node", "show", "hang_parent"]
                )
                nodes = json.loads(stdout)
                if (
                    nodes
                    and nodes[0].get("owner")
                    == "fuchsia-pkg://fuchsia.com/hang-on-stop#meta/hang-on-stop.cm"
                ):
                    bound = True
            except Exception as e:
                _LOGGER.warning(f"Error during driver binding check: {e}")

            if bound:
                _LOGGER.info(
                    "hang-on-stop driver successfully bound to hang_parent!"
                )
                break
            await asyncio.sleep(1)

        asserts.assert_true(
            bound, "Failed to bind hang-on-stop driver to hang_parent"
        )

        # 2. Trigger reboot to bootloader and wait for device to enter Fastboot.
        _LOGGER.info("Triggering reboot to bootloader...")
        self.device.fastboot.boot_to_fastboot_mode()

        # 3. Assert we are in fastboot.
        asserts.assert_true(
            self.device.fastboot.is_in_fastboot_mode(),
            "Device failed to boot into fastboot",
        )

        # 4. Recovery: Reboot back to Fuchsia.
        _LOGGER.info("Rebooting back to Fuchsia...")
        await self.device.fastboot.boot_to_fuchsia_mode()
        await self.device.wait_for_online()
        _LOGGER.info("Device is back online.")

    async def teardown_test(self) -> None:
        # Ensure we always attempt to recover the device if it got stuck in fastboot
        try:
            if self.device.fastboot.is_in_fastboot_mode():
                _LOGGER.warning(
                    "Device stuck in fastboot during teardown, attempting recovery..."
                )
                await self.device.fastboot.boot_to_fuchsia_mode()
                await self.device.wait_for_online()
        except Exception as e:
            _LOGGER.error(f"Failed to recover device in teardown: {e}")
        await super().teardown_test()


if __name__ == "__main__":
    test_runner.main()
