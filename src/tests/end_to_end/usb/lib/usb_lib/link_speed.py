# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Asserts the negotiated USB link speed of a DUT.

A link that falls back to FullSpeed (bad cable, PHY misconfiguration after a
hotplug, ...) passes every functional USB test, so check it explicitly, from
both the host's view (sysfs) and the device's view (Inspect).
"""

import logging
from typing import Any

from mobly import asserts
from usb_lib.sysfs_usb import get_usb_device_speed, wait_for_usb_device
from usb_lib.usb_config import get_dut_serial

_LOGGER: logging.Logger = logging.getLogger(__name__)

DEFAULT_EXPECTED_LINK_SPEED: str = "high"

# The speed the usb-peripheral driver reported, e.g. "full" or "high".
_DCI_SPEED_SELECTOR: str = (
    r"bootstrap/*-drivers\:*:root/usb-peripheral/dci_metrics:speed"
)


def _get_device_speed(dut: Any) -> str | None:
    for data in dut.get_inspect_data(selectors=[_DCI_SPEED_SELECTOR]).data:
        payload = data.payload or {}
        speed = (
            payload.get("root", {})
            .get("usb-peripheral", {})
            .get("dci_metrics", {})
            .get("speed")
        )
        if speed:
            return str(speed)
    return None


def assert_link_speed(
    dut: Any,
    expected: str | None = None,
    phase: str = "",
) -> None:
    """Asserts that the DUT's USB link negotiated `expected` speed.

    Fails if the host sees a different speed, or if the host and the device
    disagree. The failure message names both observed speeds. The DUT must be
    online, since its serial number and Inspect data are read over FIDL/ffx.

    Args:
        dut: Honeydew FuchsiaDevice.
        expected: Expected speed, e.g. "high". A parameter so that it can be
            set per testbed. Defaults to DEFAULT_EXPECTED_LINK_SPEED.
        phase: Optional label for messages, e.g. "iteration 3".
    """
    expected = expected or DEFAULT_EXPECTED_LINK_SPEED
    serial = get_dut_serial(dut)
    dev_node = None
    if serial:
        try:
            # Host enumeration may still be settling right after a reconnect.
            dev_node = wait_for_usb_device(target_serial=serial)
        except TimeoutError:
            pass
    host_speed = get_usb_device_speed(dev_node) if dev_node else None
    device_speed = _get_device_speed(dut)

    observed = (
        f"{f'[{phase}] ' if phase else ''}host sysfs: {host_speed!r}, "
        f"device Inspect: {device_speed!r}, expected: {expected!r}"
    )
    _LOGGER.info("USB link speed of %s: %s", dut.device_name, observed)
    asserts.assert_equal(
        host_speed, expected, f"Unexpected USB link speed ({observed})"
    )
    asserts.assert_equal(
        device_speed,
        host_speed,
        f"Host and device disagree on the USB link speed ({observed})",
    )
