# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for dmc_virtual_usb_hub.py."""

import os
import unittest
from unittest import mock

from honeydew import errors
from honeydew.auxiliary_devices.usb_power_hub import usb_power_hub
from honeydew.utils import common, host_shell
from usb_lib import (
    dmc_virtual_usb_hub,
    linux_virtual_usb_hub,
    usb_power_hub_base_test,
    usb_reset,
)

_DMC_PATH: str = "/tmp/foo/dmc"
_MOCK_OS_ENVIRON: dict[str, str] = {"DMC_PATH": _DMC_PATH}


# pylint: disable=protected-access
class DmcVirtualUsbPowerHubTests(unittest.TestCase):
    """Unit tests for dmc_virtual_usb_hub.py."""

    def setUp(self) -> None:
        super().setUp()
        with mock.patch.dict(os.environ, _MOCK_OS_ENVIRON, clear=True):
            self.hub = dmc_virtual_usb_hub.DmcVirtualUsbPowerHub(
                device_name="fuchsia-dut"
            )

    def test_init_without_dmc_path_raises_error(self) -> None:
        """Test creating the hub when DMC_PATH is not set fails."""
        with mock.patch.dict(os.environ, {}, clear=True):
            with self.assertRaisesRegex(
                usb_power_hub.UsbPowerHubError,
                "DMC_PATH environment variable is not set",
            ):
                dmc_virtual_usb_hub.DmcVirtualUsbPowerHub(
                    device_name="fuchsia-dut"
                )

    @mock.patch.object(host_shell, "run", autospec=True)
    def test_power_off(self, mock_run: mock.Mock) -> None:
        """Test power_off deauthorizes the device through DMC."""
        self.hub.power_off()
        mock_run.assert_called_once_with(
            cmd=[
                _DMC_PATH,
                "set-usb-authorized",
                "-nodename",
                "fuchsia-dut",
                "-state",
                "off",
            ]
        )

    @mock.patch.object(host_shell, "run", autospec=True)
    def test_power_on(self, mock_run: mock.Mock) -> None:
        """Test power_on authorizes the device through DMC."""
        self.hub.power_on()
        mock_run.assert_called_once_with(
            cmd=[
                _DMC_PATH,
                "set-usb-authorized",
                "-nodename",
                "fuchsia-dut",
                "-state",
                "on",
            ]
        )

    @mock.patch.object(
        host_shell,
        "run",
        side_effect=errors.HostCmdError("error"),
        autospec=True,
    )
    def test_dmc_failure_raises_error(self, mock_run: mock.Mock) -> None:
        """Test a failed DMC command raises UsbPowerHubError."""
        with self.assertRaises(usb_power_hub.UsbPowerHubError):
            self.hub.power_off()
        with self.assertRaises(usb_power_hub.UsbPowerHubError):
            self.hub.power_on()

    @mock.patch.object(common, "is_infra", return_value=True)
    @mock.patch.object(
        linux_virtual_usb_hub, "LinuxVirtualUsbAuthorizePowerHub", autospec=True
    )
    @mock.patch.object(
        dmc_virtual_usb_hub, "DmcVirtualUsbPowerHub", autospec=True
    )
    def test_usb_power_hub_base_test_lookup_virtual_hub_in_infra(
        self,
        mock_dmc_hub_cls: mock.Mock,
        mock_linux_hub_cls: mock.Mock,
        mock_is_infra: mock.Mock,
    ) -> None:
        """Test UsbPowerHubBaseTest._lookup_usb_power_hub creates DmcVirtualUsbPowerHub in infra."""
        test_obj = object.__new__(usb_power_hub_base_test.UsbPowerHubBaseTest)
        test_obj.user_params = {"use_virtual_usb_hub": True}
        mock_device = mock.MagicMock()
        mock_device.device_name = "fuchsia-dut"

        with mock.patch.object(
            usb_power_hub_base_test.UsbPowerHubBaseTest,
            "_get_device_config",
            return_value={"device_serial": "SER123"},
        ):
            hub, port = test_obj._lookup_usb_power_hub(mock_device)

        self.assertIs(hub, mock_dmc_hub_cls.return_value)
        self.assertIsNone(port)
        mock_dmc_hub_cls.assert_called_once_with(device_name="fuchsia-dut")
        mock_linux_hub_cls.assert_not_called()

    @mock.patch.object(common, "is_infra", return_value=True)
    @mock.patch.object(
        linux_virtual_usb_hub, "LinuxVirtualUsbResetPowerHub", autospec=True
    )
    @mock.patch.object(
        dmc_virtual_usb_hub, "DmcVirtualUsbResetPowerHub", autospec=True
    )
    def test_usb_power_hub_base_test_lookup_reset_hub_in_infra(
        self,
        mock_dmc_reset_hub_cls: mock.Mock,
        mock_linux_reset_hub_cls: mock.Mock,
        mock_is_infra: mock.Mock,
    ) -> None:
        """Test `virtual_usb_hub_mode: reset` creates DmcVirtualUsbResetPowerHub in infra."""
        test_obj = object.__new__(usb_power_hub_base_test.UsbPowerHubBaseTest)
        test_obj.user_params = {
            "use_virtual_usb_hub": True,
            "virtual_usb_hub_mode": "reset",
        }
        mock_device = mock.MagicMock()
        mock_device.device_name = "fuchsia-dut"

        with mock.patch.object(
            usb_power_hub_base_test.UsbPowerHubBaseTest,
            "_get_device_config",
            return_value={"device_serial": "SER123"},
        ):
            hub, port = test_obj._lookup_usb_power_hub(mock_device)

        self.assertIs(hub, mock_dmc_reset_hub_cls.return_value)
        self.assertIsNone(port)
        mock_dmc_reset_hub_cls.assert_called_once_with(
            device_name="fuchsia-dut"
        )
        mock_linux_reset_hub_cls.assert_not_called()

    @mock.patch.object(common, "is_infra", return_value=True)
    @mock.patch.object(
        linux_virtual_usb_hub, "LinuxVirtualUsbAuthorizePowerHub", autospec=True
    )
    @mock.patch.object(
        linux_virtual_usb_hub, "LinuxVirtualUsbResetPowerHub", autospec=True
    )
    @mock.patch.object(
        dmc_virtual_usb_hub, "DmcVirtualUsbPowerHub", autospec=True
    )
    @mock.patch.object(
        dmc_virtual_usb_hub, "DmcVirtualUsbResetPowerHub", autospec=True
    )
    def test_usb_power_hub_base_test_lookup_unsupported_mode_in_infra(
        self,
        mock_dmc_reset_hub_cls: mock.Mock,
        mock_dmc_hub_cls: mock.Mock,
        mock_linux_reset_hub_cls: mock.Mock,
        mock_linux_hub_cls: mock.Mock,
        mock_is_infra: mock.Mock,
    ) -> None:
        """Test an unsupported `virtual_usb_hub_mode` raises before picking a hub."""
        test_obj = object.__new__(usb_power_hub_base_test.UsbPowerHubBaseTest)
        test_obj.user_params = {
            "use_virtual_usb_hub": True,
            "virtual_usb_hub_mode": "bogus",
        }
        mock_device = mock.MagicMock()
        mock_device.device_name = "fuchsia-dut"

        with mock.patch.object(
            usb_power_hub_base_test.UsbPowerHubBaseTest,
            "_get_device_config",
            return_value={"device_serial": "SER123"},
        ):
            with self.assertRaises(ValueError):
                test_obj._lookup_usb_power_hub(mock_device)

        mock_dmc_hub_cls.assert_not_called()
        mock_dmc_reset_hub_cls.assert_not_called()
        mock_linux_hub_cls.assert_not_called()
        mock_linux_reset_hub_cls.assert_not_called()


class DmcVirtualUsbResetPowerHubTests(unittest.TestCase):
    """Unit tests for DmcVirtualUsbResetPowerHub."""

    def setUp(self) -> None:
        super().setUp()
        with mock.patch.dict(os.environ, _MOCK_OS_ENVIRON, clear=True):
            self.hub = dmc_virtual_usb_hub.DmcVirtualUsbResetPowerHub(
                device_name="fuchsia-dut"
            )

    def test_init_without_dmc_path_raises_error(self) -> None:
        """Test creating the hub when DMC_PATH is not set fails."""
        with mock.patch.dict(os.environ, {}, clear=True):
            with self.assertRaisesRegex(
                usb_power_hub.UsbPowerHubError,
                "DMC_PATH environment variable is not set",
            ):
                dmc_virtual_usb_hub.DmcVirtualUsbResetPowerHub(
                    device_name="fuchsia-dut"
                )

    @mock.patch.object(host_shell, "run", autospec=True)
    def test_reset(self, mock_run: mock.Mock) -> None:
        """Test reset resets the USB link through DMC."""
        self.hub.reset()
        mock_run.assert_called_once_with(
            cmd=[_DMC_PATH, "reset-usb-device", "-nodename", "fuchsia-dut"]
        )

    @mock.patch.object(host_shell, "run", autospec=True)
    def test_reset_usb_link_uses_reset(self, mock_run: mock.Mock) -> None:
        """Test usb_reset.reset_usb_link resets instead of power cycling."""
        self.assertIsInstance(self.hub, usb_reset.UsbResetCapable)
        usb_reset.reset_usb_link(self.hub)
        mock_run.assert_called_once_with(
            cmd=[_DMC_PATH, "reset-usb-device", "-nodename", "fuchsia-dut"]
        )

    @mock.patch.object(
        host_shell,
        "run",
        side_effect=errors.HostCmdError("error"),
        autospec=True,
    )
    def test_reset_failure_raises_error(self, mock_run: mock.Mock) -> None:
        """Test a failed DMC reset raises UsbPowerHubError."""
        with self.assertRaises(usb_power_hub.UsbPowerHubError):
            self.hub.reset()

    @mock.patch.object(host_shell, "run", autospec=True)
    def test_power_off_raises_error(self, mock_run: mock.Mock) -> None:
        """Test power_off is not supported."""
        with self.assertRaises(usb_power_hub.UsbPowerHubError):
            self.hub.power_off()
        mock_run.assert_not_called()

    @mock.patch.object(host_shell, "run", autospec=True)
    def test_power_on_is_noop(self, mock_run: mock.Mock) -> None:
        """Test power_on does nothing."""
        self.hub.power_on()
        mock_run.assert_not_called()
