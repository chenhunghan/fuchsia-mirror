# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Contains errors raised by WLAN affordances."""

from honeydew import errors


class BluetoothError(errors.HoneydewError):
    """Exception to be raised if Bluetooth operation fails."""


class BluetoothStateError(errors.HoneydewError):
    """Exception to be raised for unexpected Bluetooth states."""


# Errors raised on failure by Bluetooth operations over Fuchsia-Controller:
# - `errors.FIDL_ERRORS` from the FIDL calls.
# - `AssertionError` from `unwrap()` on a FIDL result that holds an error, and
#   from checks that the needed FIDL proxy or client is set up.
# - `TimeoutError` from the `asyncio` timeouts around the FIDL calls.
# - `HoneydewError` from Honeydew helpers, e.g. `FuchsiaControllerError` from
#   `channel_create()` or `BluetoothStateError`.
BLUETOOTH_FIDL_ERRORS: tuple[type[Exception], ...] = errors.FIDL_ERRORS + (
    AssertionError,
    TimeoutError,
    errors.HoneydewError,
)
