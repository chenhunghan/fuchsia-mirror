# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Pushes real data over the DUT's USB functions.

A control-plane ping (`ffx target echo`, `adb shell echo`) only proves that
EP0 and one small Bulk transfer work. These helpers move megabytes over CDC
(via SSH) and ADB (push and pull), so that a wedged Bulk endpoint or a stale
TRB ring fails loudly.
"""

import contextlib
import logging
import os
import tempfile
from typing import Any

from honeydew import errors as honeydew_errors
from honeydew.transports.ffx.types import MachineFormat
from mobly import asserts

_LOGGER: logging.Logger = logging.getLogger(__name__)

# Timeout of every traffic command. A stuck endpoint fails rather than hangs.
DEFAULT_COMMAND_TIMEOUT_SEC: float = 60.0
DEFAULT_TRANSFER_SIZE_BYTES: int = 5 * 1024 * 1024

# `/tmp` is an in-memory tmpfs, so ADB transfers don't hit flash storage.
_ADB_REMOTE_PATH: str = "/tmp/usb_traffic_check.bin"
_ADB_ECHO_TOKEN: str = "usb_traffic_check"


def check_cdc_traffic(
    dut: Any,
    *,
    size_bytes: int = DEFAULT_TRANSFER_SIZE_BYTES,
    timeout_sec: float = DEFAULT_COMMAND_TIMEOUT_SEC,
    phase: str = "",
) -> None:
    """Pulls `size_bytes` from the DUT over SSH, i.e. CDC.

    DWC3 endpoints are directional (even = OUT, odd = IN) and CDC's Interrupt
    endpoint is IN, so only Bulk IN endpoints can be reassigned from
    Interrupt; the SSH request exercises Bulk OUT.

    Raises:
        mobly.signals.TestFailure: Fewer bytes than asked for came back.
        honeydew.errors.HoneydewError: The SSH command failed or timed out.
    """
    if size_bytes < 2:
        raise ValueError(f"size_bytes must be at least 2, got {size_bytes}")
    prefix = f"[{phase}] " if phase else ""
    output = dut.ffx.run(
        ["target", "ssh", f'printf "X%{size_bytes - 2}sX" ""'],
        timeout=timeout_sec,
        log_output=False,
        machine=MachineFormat.RAW,
        disable_controlmaster=True,
    )
    asserts.assert_equal(
        len(output), size_bytes, f"{prefix}CDC bytes pulled over SSH"
    )
    _LOGGER.info("%sPulled %d bytes over CDC", prefix, size_bytes)


def check_adb_traffic(
    dut: Any,
    *,
    size_bytes: int = DEFAULT_TRANSFER_SIZE_BYTES,
    timeout_sec: float = DEFAULT_COMMAND_TIMEOUT_SEC,
    phase: str = "",
) -> None:
    """Round-trips `size_bytes` of random data over ADB.

    Pushes a random payload, pulls it back, and compares every byte, so that
    data corruption on either Bulk direction fails and not only a hang.

    Raises:
        mobly.signals.TestFailure: The echo or the pulled data is wrong.
        honeydew.errors.HoneydewError: An ADB command failed or timed out.
    """
    prefix = f"[{phase}] " if phase else ""

    def adb(*args: str) -> str:
        return dut.adb.run(list(args), timeout=timeout_sec, attempts=1)

    output = adb("shell", "echo", _ADB_ECHO_TOKEN)
    asserts.assert_in(_ADB_ECHO_TOKEN, output, f"{prefix}adb shell echo")
    payload = os.urandom(size_bytes)
    with tempfile.TemporaryDirectory() as tmp:
        sent = os.path.join(tmp, "sent.bin")
        received = os.path.join(tmp, "received.bin")
        with open(sent, "wb") as sent_file:
            sent_file.write(payload)
        try:
            adb("push", "-Z", sent, _ADB_REMOTE_PATH)
            adb("pull", "-Z", _ADB_REMOTE_PATH, received)
        finally:
            with contextlib.suppress(honeydew_errors.HoneydewError):
                adb("shell", "rm", "-f", _ADB_REMOTE_PATH)
        with open(received, "rb") as received_file:
            data = received_file.read()
        asserts.assert_equal(
            len(data), len(payload), f"{prefix}Pulled file size mismatch"
        )
        asserts.assert_true(
            data == payload, f"{prefix}The pulled file contents differ"
        )
    _LOGGER.info("%sRound-tripped %d bytes over ADB", prefix, size_bytes)
