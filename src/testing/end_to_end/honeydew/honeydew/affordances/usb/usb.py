# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""USB affordance implementation."""

import logging
import time
from collections.abc import Callable

from honeydew import affordances_capable, errors
from honeydew.affordances import affordance
from honeydew.affordances.usb import errors as usb_errors
from honeydew.auxiliary_devices.usb_power_hub import (
    usb_power_hub as usb_power_hub_interface,
)
from honeydew.transports.ffx import ffx as ffx_transport
from honeydew.transports.fuchsia_controller import (
    fuchsia_controller as fc_transport,
)
from honeydew.transports.serial import serial as serial_transport

_LOGGER: logging.Logger = logging.getLogger(__name__)

_USB_CLI_STOP_TOKENS: tuple[str, ...] = (
    "[usb-cli:DONE]",
    "usb-cli error:",
    "not found",
)
_USB_CLI_HEALTH_TIMEOUT_SECS: float = 5.0
_DEFAULT_USB_DISCONNECT_TIMEOUT_SECS: float = 60.0
_DEFAULT_USB_CONNECT_TIMEOUT_SECS: float = 60.0


class USB(affordance.Affordance):
    """USB affordance implementation.

    Args:
        device_name: Fuchsia device name.
        ffx: FFX transport implementation.
        fuchsia_controller: FuchsiaController transport implementation.
        reboot_affordance: Object that implements RebootCapableDevice to wait
            for device offline state and query static address status.
        device_address_change: Object that implements
            FuchsiaDeviceAddressChange to re-resolve device address after
            reconnecting USB.
        health_check: Object that implements HealthCheckCapable to verify device
            health.
        serial: Optional Serial transport implementation used for out-of-band
            USB diagnostics.
        power_hub: Optional UsbPowerHub auxiliary device implementation.
    """

    def __init__(
        self,
        device_name: str,
        ffx: ffx_transport.FFX,
        fuchsia_controller: fc_transport.FuchsiaController,
        reboot_affordance: affordances_capable.RebootCapableDevice,
        device_address_change: affordances_capable.FuchsiaDeviceAddressChange,
        health_check: affordances_capable.HealthCheckCapable,
        serial: serial_transport.Serial | None = None,
        power_hub: usb_power_hub_interface.UsbPowerHub | None = None,
    ) -> None:
        self._device_name: str = device_name
        self._ffx: ffx_transport.FFX = ffx
        self._fuchsia_controller: fc_transport.FuchsiaController = (
            fuchsia_controller
        )
        self._reboot_affordance: affordances_capable.RebootCapableDevice = (
            reboot_affordance
        )
        self._device_address_change: (
            affordances_capable.FuchsiaDeviceAddressChange
        ) = device_address_change
        self._health_check: affordances_capable.HealthCheckCapable = (
            health_check
        )
        self._serial: serial_transport.Serial | None = serial
        self._power_hub: usb_power_hub_interface.UsbPowerHub | None = power_hub
        self._before_disconnect_fns: list[Callable[[], None]] = []
        self._after_connect_fns: list[Callable[[], None]] = []

        self.verify_supported()

    def verify_supported(self) -> None:
        """Verifies that USB affordance is supported by the Fuchsia device.

        Raises:
            NotSupportedError: If affordance is not supported.
        """
        # All fuchsia devices support USB affordance.

    @property
    def power_hub(self) -> usb_power_hub_interface.UsbPowerHub | None:
        """USB power hub controlling this device, if one has been set."""
        return self._power_hub

    def set_power_hub(
        self,
        power_hub: usb_power_hub_interface.UsbPowerHub,
    ) -> None:
        """Set USB power hub for device.

        Note: This is normally populated automatically when accessing
        `FuchsiaDevice.usb` via `UsbPowerHub.from_config()` and does not need to
        be called manually. Call this only to override the auto-configured
        implementation or inject a custom/dynamically constructed instance at
        runtime (e.g., a test-specific fallback when no hub is configured).

        Args:
            power_hub: Implementation of UsbPowerHub interface.
        """
        self._power_hub = power_hub

    # TODO(b/582408121): Decide between making USB methods (`disconnect()`,
    # `connect()`, `before_disconnect()`, `after_connect()`, and
    # `register_for_*()`) async vs. keeping them sync:
    # 1. Option #1: To make these methods async, `Adb` needs to be made async
    #    because it currently calls these methods in `Adb.__init__()` (via
    #    `check_connection()` -> `run()` -> `_recover_adb_server()`), and
    #    `__init__()` does not support calling async methods. Supporting this
    #    requires converting `Adb` to `AsyncLazyReady` / `@ensure_ready`
    #    (`make_ready()`).
    # 2. Option #2: To keep these methods sync and use them in all places such
    #    as suspend-resume, callers of `register_for_*()` must pass only sync
    #    callbacks (`Callable[[], None]`). Currently WLAN has async callback
    #    functions registered on suspend-resume which can be converted to sync.
    #    Converting WLAN callbacks to sync allows keeping `USB` methods sync,
    #    with the trade-off that if any genuine async function ever needs to be
    #    passed to `USB.register_for_*()` in the future, it cannot be supported.
    # 3. For the time being, we are proceeding with Option #2 as these methods
    #    first need to be called in `Adb`. Soon, when `USB` needs to be called
    #    by suspend-resume methods (which are currently used by WLAN passing
    #    async callbacks), decide whether to convert WLAN callback functions to
    #    sync so `USB` can remain sync or convert `Adb` to async so `USB` can be
    #    converted to async.
    def _get_remaining_timeout(
        self,
        start_time: float,
        timeout: float | None,
        operation: str,
    ) -> float | None:
        """Returns the remaining timeout in seconds, or raises UsbError if expired."""
        if timeout is None:
            return None
        elapsed: float = time.monotonic() - start_time
        remaining: float = timeout - elapsed
        if remaining <= 0:
            raise usb_errors.UsbError(
                f"Timed out after {timeout}s during USB {operation} for "
                f"'{self._device_name}'"
            )
        return remaining

    def disconnect(
        self,
        timeout: float | None = _DEFAULT_USB_DISCONNECT_TIMEOUT_SECS,
    ) -> None:
        """Disconnects USB from the device.

        Calls `before_disconnect()`, powers off the USB power hub port, and
        waits for the device to go offline.

        Args:
            timeout: Maximum time in seconds to wait for the USB disconnect
                operation. Defaults to 60.0 seconds. If None, waits
                indefinitely.

        Raises:
            errors.NotSupportedError: If USB power hub is not configured.
            usb_errors.UsbError: If disconnecting USB fails or times out.
        """
        if self._power_hub is None:
            raise errors.NotSupportedError(
                f"Cannot disconnect USB for '{self._device_name}': "
                "USB power hub is not configured. Either call "
                "`usb.set_power_hub()` or ensure USB power hub details are "
                "provided in the testbed config."
            )

        _LOGGER.info("Disconnecting USB from %s...", self._device_name)
        start_time: float = time.monotonic()
        self.before_disconnect()
        try:
            self._power_hub.power_off()
            remaining_timeout: float | None = self._get_remaining_timeout(
                start_time=start_time, timeout=timeout, operation="disconnect"
            )
            self._reboot_affordance.wait_for_offline(timeout=remaining_timeout)
        except usb_errors.UsbError:
            raise
        except (
            usb_power_hub_interface.UsbPowerHubError,
            errors.HoneydewError,
        ) as err:
            raise usb_errors.UsbError(
                f"Failed to disconnect USB from '{self._device_name}'"
            ) from err

    def connect(
        self,
        timeout: float | None = _DEFAULT_USB_CONNECT_TIMEOUT_SECS,
    ) -> None:
        """Connects USB to the device.

        Powers on the USB power hub port and calls `after_connect()`.

        Args:
            timeout: Maximum time in seconds to wait for the USB connect
                operation. Defaults to 60.0 seconds. If None, waits
                 indefinitely.

        Raises:
            errors.NotSupportedError: If USB power hub is not configured.
            usb_errors.UsbError: If connecting USB fails or times out.
        """
        if self._power_hub is None:
            raise errors.NotSupportedError(
                f"Cannot connect USB to '{self._device_name}': "
                "USB power hub is not configured. Either call "
                "`usb.set_power_hub()` or ensure USB power hub details are "
                "provided in the testbed config."
            )

        _LOGGER.info("Connecting USB to %s...", self._device_name)
        start_time: float = time.monotonic()
        try:
            self._power_hub.power_on()
            remaining_timeout: float | None = self._get_remaining_timeout(
                start_time=start_time, timeout=timeout, operation="connect"
            )
            self.after_connect(timeout=remaining_timeout)
        except usb_errors.UsbError:
            raise
        except (
            usb_power_hub_interface.UsbPowerHubError,
            errors.HoneydewError,
        ) as err:
            raise usb_errors.UsbError(
                f"Failed to connect USB to '{self._device_name}'"
            ) from err

    def before_disconnect(self) -> None:
        """Notifies FFX of intentional disconnect and calls all functions registered via `register_for_before_disconnect()` in registration (FIFO) order."""
        self._ffx.notify_intentional_disconnect()
        for fn in self._before_disconnect_fns:
            _LOGGER.info("Calling %s", fn.__qualname__)
            fn()

    def after_connect(
        self,
        timeout: float | None = _DEFAULT_USB_CONNECT_TIMEOUT_SECS,
    ) -> None:
        """Re-establishes device connection and calls all functions registered via `register_for_after_connect()` in registration (FIFO) order.

        Args:
            timeout: Maximum time in seconds to wait for RCS connection after
                USB is connected. Defaults to 60.0 seconds. If None, waits
                indefinitely.

        Raises:
            usb_errors.UsbError: If re-establishing the device connection fails
                or times out.
        """
        try:
            if self._reboot_affordance.is_static_address:
                self._ffx.wait_for_rcs_connection(
                    include_target_name=False,
                    timeout=timeout,
                )
                self._fuchsia_controller.create_context()
                self._health_check.health_check()
            else:
                self._ffx.wait_for_rcs_connection(
                    include_target_name=True,
                    timeout=timeout,
                )
                self._device_address_change.resolve_device_address()
        except usb_errors.UsbError:
            raise
        except errors.HoneydewError as err:
            raise usb_errors.UsbError(
                f"Failed to re-establish connection after USB connect for "
                f"'{self._device_name}'"
            ) from err

        for fn in self._after_connect_fns:
            _LOGGER.info("Calling %s", fn.__qualname__)
            fn()

    def register_for_before_disconnect(self, fn: Callable[[], None]) -> None:
        """Registers a function that will be called in `before_disconnect()`.

        Registered functions are invoked in registration (FIFO) order.

        Args:
            fn: Function that needs to be called before USB is disconnected.
        """
        self._before_disconnect_fns.append(fn)

    def register_for_after_connect(self, fn: Callable[[], None]) -> None:
        """Registers a function that will be called in `after_connect()`.

        Registered functions are invoked in registration (FIFO) order.

        Args:
            fn: Function that needs to be called after USB is connected.
        """
        self._after_connect_fns.append(fn)

    def get_diagnostic_info(self) -> str:
        """Runs `usb-cli health` over serial out-of-band to collect USB diagnostic info.

        Returns:
            Output of `usb-cli health` over serial.

        Raises:
            errors.NotSupportedError: If serial transport is not configured.
            usb_errors.UsbError: If collecting USB diagnostic info fails.
        """
        if self._serial is None:
            raise errors.NotSupportedError(
                f"Cannot get USB diagnostic info for '{self._device_name}': "
                "serial transport is not configured."
            )
        try:
            output: str = self._serial.send_and_recv(
                cmd="usb-cli health",
                stop_tokens=_USB_CLI_STOP_TOKENS,
                timeout_sec=_USB_CLI_HEALTH_TIMEOUT_SECS,
            )
            _LOGGER.info(
                "Out-of-band `usb-cli health` over serial for %s:\n%s",
                self._device_name,
                output,
            )
            return output
        except usb_errors.UsbError:
            raise
        except errors.HoneydewError as err:
            raise usb_errors.UsbError(
                f"Failed to collect `usb-cli health` over serial for "
                f"'{self._device_name}'"
            ) from err
