# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Mobly test for Fastboot USB bulk payload staging and PHY state handoff."""

import logging
import os
import re
import tempfile
from typing import Tuple

import usb_lib
from honeydew.transports.ffx import errors as ffx_errors
from mobly import asserts, test_runner

_LOGGER: logging.Logger = logging.getLogger(__name__)

_USB_PERIPHERAL_API_NAME = "fuchsia.hardware.usb.dci.UsbDciService"

_GETVAR_REGEX = re.compile(
    r"(\(bootloader\) )?(?P<name>[a-zA-Z0-9_\-:]+): ?(?P<val>.*)"
)


def _parse_getvar_line(line: str) -> Tuple[str, str] | None:
    """Parses a single `getvar` output line into a (name, value) tuple."""
    if "waiting for" in line or not line.strip():
        return None
    match = _GETVAR_REGEX.match(line.strip())
    if match:
        return (match.group("name"), match.group("val").strip())
    return None


class UsbFastbootTest(usb_lib.UsbPowerHubBaseTest):
    """Verifies Fastboot USB bulk payload staging and PHY state handoff."""

    USB_POWER_HUB_REQUIRED: bool = False

    async def teardown_test(self) -> None:
        """Ensures device is returned safely to Fuchsia mode after each test."""
        try:
            await usb_lib.recover_to_fuchsia_mode(
                self.dut,
                usb_power_hub=self._usb_power_hub,
                usb_port=self._usb_port,
                timeout_sec=60,
            )
        except Exception as e:
            _LOGGER.warning("Error recovering device during teardown: %s", e)
        finally:
            await super().teardown_test()

    async def test_fastboot_stage_payload(self) -> None:
        """Verifies bounded USB bulk payload staging in Fastboot mode."""
        _LOGGER.info("Booting DUT to Fastboot mode...")
        await usb_lib.reboot_to_fastboot_mode(
            self.dut,
            usb_power_hub=self._usb_power_hub,
            usb_port=self._usb_port,
            timeout_sec=30,
        )

        asserts.assert_true(
            await self.dut.fastboot.is_in_fastboot_mode(),
            msg=f"{self.dut.device_name} failed to enter fastboot mode.",
        )

        _LOGGER.info(
            "Querying Fastboot `max-download-size` to bound bulk payload staging..."
        )
        max_dl_lines = await self.dut.fastboot.run(
            ["getvar", "max-download-size"]
        )
        max_dl_parsed: list[Tuple[str, str]] = [
            res
            for line in max_dl_lines
            if (res := _parse_getvar_line(line)) is not None
        ]
        asserts.assert_true(
            len(max_dl_parsed) > 0,
            msg="Failed to parse max-download-size from fastboot getvar query",
        )
        _LOGGER.info("Parsed max-download-size: %s", max_dl_parsed[0])

        # Stage a safe 4MB payload, bounded by max-download-size, to thoroughly
        # exercise USB bulk OUT transfers without risking bootloader RAM exhaustion or timeouts (b/553616205).
        max_dl_bytes = 0
        if max_dl_parsed:
            max_dl_val_str = max_dl_parsed[0][1].strip()
            try:
                if max_dl_val_str.lower().startswith("0x"):
                    max_dl_bytes = int(max_dl_val_str, 16)
                else:
                    max_dl_bytes = int(max_dl_val_str)
            except ValueError:
                max_dl_bytes = 0

        stage_size_bytes = 4 * 1024 * 1024
        if max_dl_bytes > 0:
            stage_size_bytes = min(stage_size_bytes, max_dl_bytes // 2)

        _LOGGER.info(
            "Generating %d-byte payload for Fastboot staging...",
            stage_size_bytes,
        )
        with tempfile.NamedTemporaryFile(delete=False) as f:
            f.write(os.urandom(stage_size_bytes))
            temp_path = f.name

        try:
            _LOGGER.info(
                "Staging %d-byte payload %s to Fastboot memory buffer via USB...",
                stage_size_bytes,
                temp_path,
            )
            # Run 'stage' command using the underlying ffx target fastboot execution.
            await self.dut.fastboot.run(["stage", temp_path])
            _LOGGER.info(
                "Successfully staged payload over USB without flashing."
            )
        finally:
            os.remove(temp_path)

        _LOGGER.info("Rebooting DUT back to Fuchsia mode...")
        await usb_lib.recover_to_fuchsia_mode(
            self.dut,
            usb_power_hub=self._usb_power_hub,
            usb_port=self._usb_port,
            timeout_sec=60,
        )

    async def test_phy_state_handoff_and_reenumeration(self) -> None:
        """Verifies seamless USB PHY state handoff and driver re-enumeration after boot to Zircon."""
        _LOGGER.info(
            "Booting DUT to Fastboot mode to test PHY state handoff..."
        )
        await usb_lib.reboot_to_fastboot_mode(
            self.dut,
            usb_power_hub=self._usb_power_hub,
            usb_port=self._usb_port,
            timeout_sec=30,
        )

        asserts.assert_true(
            await self.dut.fastboot.is_in_fastboot_mode(),
            msg=f"{self.dut.device_name} failed to enter fastboot mode.",
        )

        _LOGGER.info(
            "Rebooting DUT out of Fastboot to trigger PHY handoff to Zircon..."
        )
        await usb_lib.recover_to_fuchsia_mode(
            self.dut,
            usb_power_hub=self._usb_power_hub,
            usb_port=self._usb_port,
            timeout_sec=60,
        )

        _LOGGER.info("Verifying DUT is online in Fuchsia mode...")
        await self.dut.wait_for_online()

        # TODO(b/556242644): Refactor duplicated USB driver and inspect verification
        # into a shared usb_health_check helper once utilities are aligned between
        # Sorrel and Iris.
        _LOGGER.info("Executing diagnostics: checking USB peripheral driver...")
        driver_list = self.dut.ffx.run(["driver", "list-devices", "-v"])
        asserts.assert_in(
            _USB_PERIPHERAL_API_NAME,
            driver_list,
            msg=f"Expected {_USB_PERIPHERAL_API_NAME} driver to be active after PHY handoff",
        )

        _LOGGER.info(
            "Executing diagnostics: ffx inspect show for usb-policy..."
        )
        cli_output = ""
        for moniker in ["core/usb-policy", "bootstrap/boot-drivers"]:
            try:
                cli_output += self.dut.ffx.run(["inspect", "show", moniker])
            except ffx_errors.FfxCommandError as e:
                _LOGGER.debug("Inspect query on %s failed: %s", moniker, e)

        if "usb_state_history" not in cli_output:
            _LOGGER.info(
                "Not in core/usb-policy or boot-drivers, checking entire Inspect tree..."
            )
            try:
                cli_output += self.dut.ffx.run(["inspect", "show"])
            except ffx_errors.FfxCommandError as e:
                _LOGGER.debug("Full inspect show failed: %s", e)

        asserts.assert_in(
            "usb_state_history",
            cli_output,
            msg="Expected usb_state_history in ffx inspect show after PHY handoff",
        )
        _LOGGER.info(
            "Successfully verified USB PHY state handoff and re-enumeration."
        )


if __name__ == "__main__":
    test_runner.main()
