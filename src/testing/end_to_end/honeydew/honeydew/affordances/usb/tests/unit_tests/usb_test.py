# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for usb.py."""

import time
import unittest
from unittest import mock

from honeydew import affordances_capable, errors
from honeydew.affordances.usb import errors as usb_errors
from honeydew.affordances.usb import usb
from honeydew.auxiliary_devices.usb_power_hub import (
    usb_power_hub as usb_power_hub_interface,
)
from honeydew.transports.ffx import ffx as ffx_transport
from honeydew.transports.fuchsia_controller import (
    fuchsia_controller as fc_transport,
)
from honeydew.transports.serial import errors as serial_errors
from honeydew.transports.serial import serial as serial_transport

_DEVICE_NAME: str = "fuchsia-emulator"


class UsbTests(unittest.TestCase):
    """Unit tests for honeydew.affordances.usb.usb."""

    def setUp(self) -> None:
        super().setUp()
        self.mock_ffx = mock.create_autospec(ffx_transport.FFX, instance=True)
        self.mock_fc = mock.create_autospec(
            fc_transport.FuchsiaController, instance=True
        )
        self.mock_reboot_affordance = mock.create_autospec(
            affordances_capable.RebootCapableDevice, instance=True
        )
        self.mock_reboot_affordance.is_static_address = False
        self.mock_device_address_change = mock.create_autospec(
            affordances_capable.FuchsiaDeviceAddressChange, instance=True
        )
        self.mock_health_check = mock.create_autospec(
            affordances_capable.HealthCheckCapable, instance=True
        )
        self.mock_serial = mock.create_autospec(
            serial_transport.Serial, instance=True
        )
        self.mock_power_hub = mock.create_autospec(
            usb_power_hub_interface.UsbPowerHub, instance=True
        )
        self.usb_obj = usb.USB(
            device_name=_DEVICE_NAME,
            ffx=self.mock_ffx,
            fuchsia_controller=self.mock_fc,
            reboot_affordance=self.mock_reboot_affordance,
            device_address_change=self.mock_device_address_change,
            health_check=self.mock_health_check,
            serial=self.mock_serial,
            power_hub=self.mock_power_hub,
        )

    def test_verify_supported_called_in_init(self) -> None:
        """Verifies that __init__() calls verify_supported()."""
        with mock.patch.object(
            usb.USB, "verify_supported", autospec=True
        ) as mock_verify_supported:
            usb.USB(
                device_name=_DEVICE_NAME,
                ffx=self.mock_ffx,
                fuchsia_controller=self.mock_fc,
                reboot_affordance=self.mock_reboot_affordance,
                device_address_change=self.mock_device_address_change,
                health_check=self.mock_health_check,
            )
        mock_verify_supported.assert_called_once()

    def test_set_power_hub(self) -> None:
        """Verifies set_power_hub() updates power_hub."""
        usb_without_hub = usb.USB(
            device_name=_DEVICE_NAME,
            ffx=self.mock_ffx,
            fuchsia_controller=self.mock_fc,
            reboot_affordance=self.mock_reboot_affordance,
            device_address_change=self.mock_device_address_change,
            health_check=self.mock_health_check,
        )
        self.assertIsNone(usb_without_hub.power_hub)

        usb_without_hub.set_power_hub(power_hub=self.mock_power_hub)

        self.assertIs(usb_without_hub.power_hub, self.mock_power_hub)

    def test_disconnect_success(self) -> None:
        """Verifies disconnect() calls before_disconnect(), power_off(), and wait_for_offline() in order."""
        call_order: list[str] = []
        self.mock_ffx.notify_intentional_disconnect.side_effect = (
            lambda: call_order.append("notify_intentional_disconnect")
        )
        self.usb_obj.register_for_before_disconnect(
            lambda: call_order.append("before_disconnect")
        )
        self.mock_power_hub.power_off.side_effect = lambda: call_order.append(
            "power_off"
        )
        self.mock_reboot_affordance.wait_for_offline.side_effect = (
            lambda timeout=None: call_order.append("wait_for_offline")
        )

        with mock.patch.object(time, "monotonic", side_effect=[100.0, 105.0]):
            self.usb_obj.disconnect()

        self.assertEqual(
            call_order,
            [
                "notify_intentional_disconnect",
                "before_disconnect",
                "power_off",
                "wait_for_offline",
            ],
        )
        self.mock_ffx.notify_intentional_disconnect.assert_called_once()
        self.mock_power_hub.power_off.assert_called_once_with()
        self.mock_reboot_affordance.wait_for_offline.assert_called_once_with(
            timeout=55.0
        )

    def test_disconnect_with_timeout_none(self) -> None:
        """Verifies disconnect(timeout=None) passes timeout=None to wait_for_offline()."""
        self.usb_obj.disconnect(timeout=None)
        self.mock_reboot_affordance.wait_for_offline.assert_called_once_with(
            timeout=None
        )

    def test_disconnect_without_power_hub_raises_not_supported_error(
        self,
    ) -> None:
        """Verifies disconnect() raises NotSupportedError when power_hub is None."""
        usb_without_hub = usb.USB(
            device_name=_DEVICE_NAME,
            ffx=self.mock_ffx,
            fuchsia_controller=self.mock_fc,
            reboot_affordance=self.mock_reboot_affordance,
            device_address_change=self.mock_device_address_change,
            health_check=self.mock_health_check,
        )
        with self.assertRaises(errors.NotSupportedError):
            usb_without_hub.disconnect()

    def test_disconnect_wraps_usb_power_hub_error(self) -> None:
        """Verifies disconnect() wraps UsbPowerHubError in UsbError and skips wait_for_offline()."""
        self.mock_power_hub.power_off.side_effect = (
            usb_power_hub_interface.UsbPowerHubError("power_off failed")
        )
        with self.assertRaises(usb_errors.UsbError):
            self.usb_obj.disconnect()
        self.mock_reboot_affordance.wait_for_offline.assert_not_called()

    def test_disconnect_wraps_wait_for_offline_error(self) -> None:
        """Verifies disconnect() wraps HoneydewError from wait_for_offline() in UsbError."""
        self.mock_reboot_affordance.wait_for_offline.side_effect = (
            errors.FuchsiaDeviceError("wait_for_offline failed")
        )
        with self.assertRaises(usb_errors.UsbError):
            self.usb_obj.disconnect()

    def test_disconnect_timeout_expired_before_wait_for_offline(self) -> None:
        """Verifies disconnect() raises UsbError if timeout expires before wait_for_offline()."""
        with mock.patch.object(time, "monotonic", side_effect=[100.0, 110.0]):
            with self.assertRaises(usb_errors.UsbError):
                self.usb_obj.disconnect(timeout=10.0)
        self.mock_reboot_affordance.wait_for_offline.assert_not_called()

    def test_connect_success_dynamic_address(self) -> None:
        """Verifies connect() calls power_on(), wait_for_rcs_connection(), resolve_device_address(), and after_connect() when address is not static."""
        self.mock_reboot_affordance.is_static_address = False
        call_order: list[str] = []
        self.mock_power_hub.power_on.side_effect = lambda: call_order.append(
            "power_on"
        )
        self.mock_ffx.wait_for_rcs_connection.side_effect = (
            lambda include_target_name, timeout=None: call_order.append(
                f"wait_for_rcs_connection:{include_target_name}:{timeout}"
            )
        )
        self.mock_device_address_change.resolve_device_address.side_effect = (
            lambda: call_order.append("resolve_device_address")
        )
        self.usb_obj.register_for_after_connect(
            lambda: call_order.append("after_connect")
        )

        with mock.patch.object(time, "monotonic", side_effect=[100.0, 102.0]):
            self.usb_obj.connect()

        self.assertEqual(
            call_order,
            [
                "power_on",
                "wait_for_rcs_connection:True:58.0",
                "resolve_device_address",
                "after_connect",
            ],
        )
        self.mock_power_hub.power_on.assert_called_once_with()
        self.mock_ffx.wait_for_rcs_connection.assert_called_once_with(
            include_target_name=True, timeout=58.0
        )
        self.mock_device_address_change.resolve_device_address.assert_called_once()
        self.mock_fc.create_context.assert_not_called()
        self.mock_health_check.health_check.assert_not_called()

    def test_connect_success_static_address(self) -> None:
        """Verifies connect() calls power_on(), wait_for_rcs_connection(), create_context(), health_check(), and after_connect() when address is static."""
        self.mock_reboot_affordance.is_static_address = True
        call_order: list[str] = []
        self.mock_power_hub.power_on.side_effect = lambda: call_order.append(
            "power_on"
        )
        self.mock_ffx.wait_for_rcs_connection.side_effect = (
            lambda include_target_name, timeout=None: call_order.append(
                f"wait_for_rcs_connection:{include_target_name}:{timeout}"
            )
        )
        self.mock_fc.create_context.side_effect = lambda: call_order.append(
            "create_context"
        )
        self.mock_health_check.health_check.side_effect = (
            lambda: call_order.append("health_check")
        )
        self.usb_obj.register_for_after_connect(
            lambda: call_order.append("after_connect")
        )

        with mock.patch.object(time, "monotonic", side_effect=[100.0, 104.0]):
            self.usb_obj.connect()

        self.assertEqual(
            call_order,
            [
                "power_on",
                "wait_for_rcs_connection:False:56.0",
                "create_context",
                "health_check",
                "after_connect",
            ],
        )
        self.mock_power_hub.power_on.assert_called_once_with()
        self.mock_ffx.wait_for_rcs_connection.assert_called_once_with(
            include_target_name=False, timeout=56.0
        )
        self.mock_device_address_change.resolve_device_address.assert_not_called()
        self.mock_fc.create_context.assert_called_once()
        self.mock_health_check.health_check.assert_called_once()

    def test_connect_with_timeout_none(self) -> None:
        """Verifies connect(timeout=None) passes timeout=None to wait_for_rcs_connection()."""
        self.usb_obj.connect(timeout=None)
        self.mock_ffx.wait_for_rcs_connection.assert_called_once_with(
            include_target_name=True, timeout=None
        )

    def test_connect_timeout_expired_before_after_connect(self) -> None:
        """Verifies connect() raises UsbError if timeout expires before after_connect()."""
        with mock.patch.object(time, "monotonic", side_effect=[100.0, 115.0]):
            with self.assertRaises(usb_errors.UsbError):
                self.usb_obj.connect(timeout=10.0)
        self.mock_ffx.wait_for_rcs_connection.assert_not_called()

    def test_connect_without_power_hub_raises_not_supported_error(
        self,
    ) -> None:
        """Verifies connect() raises NotSupportedError when power_hub is None."""
        usb_without_hub = usb.USB(
            device_name=_DEVICE_NAME,
            ffx=self.mock_ffx,
            fuchsia_controller=self.mock_fc,
            reboot_affordance=self.mock_reboot_affordance,
            device_address_change=self.mock_device_address_change,
            health_check=self.mock_health_check,
        )
        with self.assertRaises(errors.NotSupportedError):
            usb_without_hub.connect()

    def test_connect_wraps_usb_power_hub_error(self) -> None:
        """Verifies connect() wraps UsbPowerHubError in UsbError and skips after_connect()."""
        after_connect_cb = mock.MagicMock(__qualname__="after_connect_cb")
        self.usb_obj.register_for_after_connect(after_connect_cb)
        self.mock_power_hub.power_on.side_effect = (
            usb_power_hub_interface.UsbPowerHubError("power_on failed")
        )
        with self.assertRaises(usb_errors.UsbError):
            self.usb_obj.connect()
        self.mock_ffx.wait_for_rcs_connection.assert_not_called()
        self.mock_device_address_change.resolve_device_address.assert_not_called()
        self.mock_fc.create_context.assert_not_called()
        self.mock_health_check.health_check.assert_not_called()
        after_connect_cb.assert_not_called()

    def test_register_and_call_before_disconnect(self) -> None:
        """Verifies before_disconnect() calls notify_intentional_disconnect() and all registered callbacks in order."""
        calls: list[str] = []
        self.mock_ffx.notify_intentional_disconnect.side_effect = (
            lambda: calls.append("notify_intentional_disconnect")
        )
        self.usb_obj.register_for_before_disconnect(lambda: calls.append("cb1"))
        self.usb_obj.register_for_before_disconnect(lambda: calls.append("cb2"))

        self.usb_obj.before_disconnect()

        self.assertEqual(
            calls,
            ["notify_intentional_disconnect", "cb1", "cb2"],
        )

    def test_register_and_call_after_connect(self) -> None:
        """Verifies after_connect() re-establishes connection and invokes all registered callbacks in order."""
        calls: list[str] = []
        self.mock_ffx.wait_for_rcs_connection.side_effect = (
            lambda include_target_name, timeout=None: calls.append(
                f"wait_for_rcs_connection:{include_target_name}:{timeout}"
            )
        )
        self.mock_device_address_change.resolve_device_address.side_effect = (
            lambda: calls.append("resolve_device_address")
        )
        self.usb_obj.register_for_after_connect(lambda: calls.append("cb1"))
        self.usb_obj.register_for_after_connect(lambda: calls.append("cb2"))

        self.usb_obj.after_connect()

        self.assertEqual(
            calls,
            [
                f"wait_for_rcs_connection:True:{usb._DEFAULT_USB_CONNECT_TIMEOUT_SECS}",  # pylint: disable=protected-access
                "resolve_device_address",
                "cb1",
                "cb2",
            ],
        )

    def test_after_connect_wraps_honeydew_error(self) -> None:
        """Verifies after_connect() wraps HoneydewError in UsbError and skips callbacks."""
        after_connect_cb = mock.MagicMock(__qualname__="after_connect_cb")
        self.usb_obj.register_for_after_connect(after_connect_cb)
        self.mock_ffx.wait_for_rcs_connection.side_effect = (
            errors.DeviceNotConnectedError("not connected")
        )
        with self.assertRaises(usb_errors.UsbError):
            self.usb_obj.after_connect()
        after_connect_cb.assert_not_called()

    def test_get_diagnostic_info_success(self) -> None:
        """Verifies get_diagnostic_info() runs `usb-cli health` over serial and returns output."""
        expected_output = "USB Policy: OK\n[usb-cli:DONE]"
        self.mock_serial.send_and_recv.return_value = expected_output

        self.assertEqual(self.usb_obj.get_diagnostic_info(), expected_output)
        self.mock_serial.send_and_recv.assert_called_once_with(
            cmd="usb-cli health",
            stop_tokens=usb._USB_CLI_STOP_TOKENS,  # pylint: disable=protected-access
            timeout_sec=usb._USB_CLI_HEALTH_TIMEOUT_SECS,  # pylint: disable=protected-access
        )

    def test_get_diagnostic_info_without_serial(self) -> None:
        """Verifies get_diagnostic_info() raises NotSupportedError when serial is None."""
        usb_without_serial = usb.USB(
            device_name=_DEVICE_NAME,
            ffx=self.mock_ffx,
            fuchsia_controller=self.mock_fc,
            reboot_affordance=self.mock_reboot_affordance,
            device_address_change=self.mock_device_address_change,
            health_check=self.mock_health_check,
            serial=None,
        )
        with self.assertRaises(errors.NotSupportedError):
            usb_without_serial.get_diagnostic_info()

    def test_get_diagnostic_info_serial_error(self) -> None:
        """Verifies get_diagnostic_info() wraps HoneydewError in UsbError."""
        self.mock_serial.send_and_recv.side_effect = serial_errors.SerialError(
            "serial failed"
        )
        with self.assertRaises(usb_errors.UsbError):
            self.usb_obj.get_diagnostic_info()
        self.mock_serial.send_and_recv.assert_called_once()


if __name__ == "__main__":
    unittest.main()
