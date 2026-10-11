# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for honeydew.device_classes.android_device.py."""

import ipaddress
import unittest
from typing import Any
from unittest import mock

import fuchsia_controller_py as fuchsia_controller
from honeydew import errors
from honeydew.device_classes import android_device
from honeydew.fuchsia_device import fuchsia_device
from honeydew.transports.adb import adb as adb_transport
from honeydew.transports.adb import errors as adb_errors
from honeydew.transports.ffx import config as ffx_config
from honeydew.transports.ffx import errors as ffx_errors
from honeydew.transports.ffx import ffx
from honeydew.transports.fuchsia_controller import (
    fuchsia_controller as fc_transport,
)
from honeydew.transports.sl4f import sl4f as sl4f_transport
from honeydew.typing import custom_types

# pylint: disable=protected-access

_IPV6: str = "fe80::4fce:3102:ef13:888c%qemu"
_IPV6_OBJ: ipaddress.IPv6Address = ipaddress.IPv6Address(_IPV6)
_SSH_ADDRESS: ipaddress.IPv6Address = _IPV6_OBJ
_SSH_PORT = 8022
_TARGET_SSH_ADDRESS = custom_types.IpPort(ip=_SSH_ADDRESS, port=_SSH_PORT)

_INPUT_ARGS: dict[str, Any] = {
    "device_name": "fuchsia-android-device",
    "device_ip": _TARGET_SSH_ADDRESS,
    "device_serial_socket": "/tmp/socket",
    "ffx_config_data": ffx_config.FfxConfigData(
        isolate_dir=fuchsia_controller.IsolateDir("/tmp/isolate"),
        logs_dir="/tmp/logs",
        binary_path="/bin/ffx",
        logs_level="debug",
        enable_usb=False,
        usb_socket_path=None,
        usb_driver_autostart=False,
        subtools_search_path=None,
        proxy_timeout_secs=None,
        ssh_keepalive_timeout=None,
        emu_instance_dir=None,
        ssh_private_keys=None,
        ssh_public_keys=None,
        shared_data="/tmp/shared_data",
    ),
}


class AndroidDeviceTests(unittest.IsolatedAsyncioTestCase):
    """Unit tests for honeydew.device_classes.android_device.py."""

    def setUp(self) -> None:
        adb_binary_patcher = mock.patch.object(
            adb_transport,
            "_get_adb_binary",
            return_value="/bin/adb",
            autospec=True,
        )
        adb_binary_patcher.start()
        self.addCleanup(adb_binary_patcher.stop)

        adb_server_patcher = mock.patch.object(
            adb_transport,
            "AdbServer",
            autospec=True,
        )
        adb_server_patcher.start()
        self.addCleanup(adb_server_patcher.stop)

        adb_cache_pid_patcher = mock.patch.object(
            adb_transport.Adb,
            "_cache_adbd_pid",
            autospec=True,
        )
        adb_cache_pid_patcher.start()
        self.addCleanup(adb_cache_pid_patcher.stop)

        with (
            mock.patch.object(
                ffx.FFX,
                "_check_running_monitor",
                return_value=False,
                autospec=True,
            ),
            mock.patch.object(
                fc_transport.FuchsiaController,
                "create_context",
                autospec=True,
            ),
            mock.patch.object(
                ffx.FFX,
                "check_connection",
                autospec=True,
            ),
            mock.patch.object(
                fc_transport.FuchsiaController,
                "check_connection",
                autospec=True,
            ),
            mock.patch.object(
                adb_transport.Adb,
                "verify_supported",
                autospec=True,
            ),
            mock.patch.object(
                adb_transport.Adb,
                "check_connection",
                autospec=True,
            ),
        ):
            self.ad_obj = android_device.AndroidDevice(
                device_info=custom_types.DeviceInfo(
                    name=_INPUT_ARGS["device_name"],
                    serial_number="12345678",
                    ip_port=_INPUT_ARGS["device_ip"],
                    serial_socket=_INPUT_ARGS["device_serial_socket"],
                ),
                ffx_config_data=_INPUT_ARGS["ffx_config_data"],
            )
            self.ad_obj.__dict__.pop("adb", None)

    def test_device_hierarchy(self) -> None:
        """Test case to make sure AndroidDevice inherits properly."""
        self.assertIsInstance(self.ad_obj, fuchsia_device.FuchsiaDevice)
        self.assertIsInstance(self.ad_obj, android_device.AndroidDevice)

    def test_adb_transport_disabled(self) -> None:
        """Test case to make sure AndroidDevice raises NotEnabledError when adb is disabled."""
        config = {
            "transports": {
                "adb": {
                    "enabled": False,
                }
            }
        }
        with (
            mock.patch.object(
                ffx.FFX,
                "check_connection",
                autospec=True,
            ),
            mock.patch.object(
                fc_transport.FuchsiaController,
                "check_connection",
                autospec=True,
            ),
            mock.patch.object(
                fc_transport.FuchsiaController,
                "create_context",
                autospec=True,
            ),
        ):
            ad_obj = android_device.AndroidDevice(
                device_info=custom_types.DeviceInfo(
                    name=_INPUT_ARGS["device_name"],
                    serial_number=None,
                    ip_port=_INPUT_ARGS["device_ip"],
                    serial_socket=_INPUT_ARGS["device_serial_socket"],
                ),
                ffx_config_data=_INPUT_ARGS["ffx_config_data"],
                config=config,
            )
            with self.assertRaises(errors.NotEnabledError):
                _ = ad_obj.adb

    @mock.patch.object(
        adb_transport.Adb,
        "check_connection",
        autospec=True,
    )
    @mock.patch.object(
        adb_transport.Adb,
        "verify_supported",
        autospec=True,
    )
    def test_adb_transport(
        self,
        mock_verify_supported: mock.Mock,
        mock_check_connection: mock.Mock,
    ) -> None:
        """Test case to make sure AndroidDevice supports adb transport."""
        self.assertIsInstance(
            self.ad_obj.adb,
            adb_transport.Adb,
        )
        mock_verify_supported.assert_called_once()
        mock_check_connection.assert_called_once()
        self.ad_obj.__dict__.pop("adb", None)

    @mock.patch.object(
        adb_transport.Adb,
        "verify_supported",
        side_effect=errors.NotSupportedError("Not supported"),
        autospec=True,
    )
    def test_adb_transport_not_supported(
        self, mock_verify_supported: mock.Mock
    ) -> None:
        """Test case to make sure AndroidDevice raises NotSupportedError when
        ADB is not supported on the target."""
        with self.assertRaises(errors.NotSupportedError):
            _: adb_transport.Adb = self.ad_obj.adb
        mock_verify_supported.assert_called_once()

    @mock.patch.object(
        adb_transport.Adb,
        "check_connection",
        autospec=True,
    )
    @mock.patch.object(
        adb_transport.Adb,
        "verify_supported",
        autospec=True,
    )
    @mock.patch.object(
        ffx.FFX,
        "serial_number",
        new_callable=mock.PropertyMock,
        return_value="ffx-serial-1234",
    )
    def test_adb_transport_fallback_ffx_serial(
        self,
        mock_ffx_serial_number: mock.Mock,
        mock_verify_supported: mock.Mock,
        mock_check_connection: mock.Mock,
    ) -> None:
        """Test case to make sure AndroidDevice falls back to FFX serial_number
        when serial_number is not in device_info."""
        device_info: custom_types.DeviceInfo = self.ad_obj._device_info
        self.ad_obj._device_info = custom_types.DeviceInfo(
            name=_INPUT_ARGS["device_name"],
            serial_number=None,
            ip_port=None,
            serial_socket=None,
        )

        adb_inst: adb_transport.Adb = self.ad_obj.adb
        self.assertIsInstance(adb_inst, adb_transport.Adb)
        mock_ffx_serial_number.assert_called_once()
        mock_verify_supported.assert_called_once()
        mock_check_connection.assert_called_once()

        self.ad_obj.__dict__.pop("adb", None)
        self.ad_obj._device_info = device_info

    @mock.patch.object(
        ffx.FFX,
        "serial_number",
        new_callable=mock.PropertyMock,
        return_value=None,
    )
    def test_adb_transport_ffx_serial_none(
        self,
        mock_ffx_serial_number: mock.Mock,
    ) -> None:
        """Test case to make sure AndroidDevice raises NotSupportedError when
        FFX serial_number is None."""
        device_info: custom_types.DeviceInfo = self.ad_obj._device_info
        self.ad_obj._device_info = custom_types.DeviceInfo(
            name=_INPUT_ARGS["device_name"],
            serial_number=None,
            ip_port=None,
            serial_socket=None,
        )

        with self.assertRaises(errors.NotSupportedError):
            _: adb_transport.Adb = self.ad_obj.adb

        mock_ffx_serial_number.assert_called_once()
        self.ad_obj.__dict__.pop("adb", None)
        self.ad_obj._device_info = device_info

    @mock.patch.object(
        ffx.FFX,
        "serial_number",
        new_callable=mock.PropertyMock,
        side_effect=ffx_errors.FfxCommandError("FFX error"),
    )
    def test_adb_transport_error(
        self,
        mock_ffx_serial_number: mock.Mock,
    ) -> None:
        """Test case to make sure AndroidDevice raises NotSupportedError when we try to
        access "adb" transport without serial_number and FFX fails."""
        device_info: custom_types.DeviceInfo = self.ad_obj._device_info
        self.ad_obj._device_info = custom_types.DeviceInfo(
            name=_INPUT_ARGS["device_name"],
            serial_number=None,
            ip_port=None,
            serial_socket=None,
        )

        with self.assertRaises(errors.NotSupportedError):
            _: adb_transport.Adb = self.ad_obj.adb

        mock_ffx_serial_number.assert_called_once()
        self.ad_obj.__dict__.pop("adb", None)
        self.ad_obj._device_info = device_info

    @mock.patch.object(
        adb_transport.Adb,
        "check_connection",
        autospec=True,
    )
    @mock.patch.object(
        adb_transport.Adb,
        "verify_supported",
        autospec=True,
    )
    @mock.patch.object(
        sl4f_transport.SL4F,
        "check_connection",
        autospec=True,
    )
    @mock.patch.object(
        fc_transport.FuchsiaController,
        "check_connection",
        autospec=True,
    )
    @mock.patch.object(ffx.FFX, "check_connection", autospec=True)
    def test_health_check_adb_supported(
        self,
        mock_ffx_check_connection: mock.Mock,
        mock_fc_check_connection: mock.Mock,
        mock_sl4f_check_connection: mock.Mock,
        mock_adb_verify_supported: mock.Mock,
        mock_adb_check_connection: mock.Mock,
    ) -> None:
        """Testcase for AndroidDevice.health_check() when ADB is supported."""
        _ = self.ad_obj.adb
        mock_adb_check_connection.reset_mock()

        self.ad_obj.health_check()

        mock_ffx_check_connection.assert_called_once_with(self.ad_obj.ffx)
        mock_fc_check_connection.assert_called_once_with(
            self.ad_obj.fuchsia_controller
        )
        mock_sl4f_check_connection.assert_not_called()
        mock_adb_check_connection.assert_called_once_with(self.ad_obj.adb)

        self.ad_obj.__dict__.pop("adb", None)

    @mock.patch.object(
        adb_transport.Adb,
        "verify_supported",
        side_effect=errors.NotSupportedError("ADB not supported"),
        autospec=True,
    )
    @mock.patch.object(
        sl4f_transport.SL4F,
        "check_connection",
        autospec=True,
    )
    @mock.patch.object(
        fc_transport.FuchsiaController,
        "check_connection",
        autospec=True,
    )
    @mock.patch.object(ffx.FFX, "check_connection", autospec=True)
    def test_health_check_adb_not_supported(
        self,
        mock_ffx_check_connection: mock.Mock,
        mock_fc_check_connection: mock.Mock,
        mock_sl4f_check_connection: mock.Mock,
        mock_adb_verify_supported: mock.Mock,
    ) -> None:
        """Testcase for AndroidDevice.health_check() raising NotSupportedError when ADB is not supported."""
        with self.assertRaises(errors.NotSupportedError):
            self.ad_obj.health_check()

        mock_ffx_check_connection.assert_called_once_with(self.ad_obj.ffx)
        mock_fc_check_connection.assert_called_once_with(
            self.ad_obj.fuchsia_controller
        )
        mock_sl4f_check_connection.assert_not_called()
        mock_adb_verify_supported.assert_called_once()
        self.ad_obj.__dict__.pop("adb", None)

    @mock.patch.object(
        sl4f_transport.SL4F,
        "check_connection",
        autospec=True,
    )
    @mock.patch.object(
        fc_transport.FuchsiaController,
        "check_connection",
        autospec=True,
    )
    @mock.patch.object(ffx.FFX, "check_connection", autospec=True)
    def test_health_check_adb_disabled(
        self,
        mock_ffx_check_connection: mock.Mock,
        mock_fc_check_connection: mock.Mock,
        mock_sl4f_check_connection: mock.Mock,
    ) -> None:
        """Testcase for AndroidDevice.health_check() when ADB is disabled in config."""
        orig_config = self.ad_obj._config
        self.ad_obj._config = {
            "transports": {
                "adb": {
                    "enabled": False,
                }
            }
        }
        self.ad_obj.__dict__.pop("adb", None)
        try:
            self.ad_obj.health_check()

            mock_ffx_check_connection.assert_called_once_with(self.ad_obj.ffx)
            mock_fc_check_connection.assert_called_once_with(
                self.ad_obj.fuchsia_controller
            )
            mock_sl4f_check_connection.assert_not_called()
        finally:
            self.ad_obj._config = orig_config
            self.ad_obj.__dict__.pop("adb", None)

    @mock.patch.object(
        adb_transport.Adb,
        "check_connection",
        side_effect=adb_errors.AdbConnectionError("ADB connection error"),
        autospec=True,
    )
    @mock.patch.object(
        adb_transport.Adb,
        "verify_supported",
        autospec=True,
    )
    @mock.patch.object(
        sl4f_transport.SL4F,
        "check_connection",
        autospec=True,
    )
    @mock.patch.object(
        fc_transport.FuchsiaController,
        "check_connection",
        autospec=True,
    )
    @mock.patch.object(ffx.FFX, "check_connection", autospec=True)
    def test_health_check_adb_connection_error(
        self,
        mock_ffx_check_connection: mock.Mock,
        mock_fc_check_connection: mock.Mock,
        mock_sl4f_check_connection: mock.Mock,
        mock_adb_verify_supported: mock.Mock,
        mock_adb_check_connection: mock.Mock,
    ) -> None:
        """Testcase for AndroidDevice.health_check() raising HealthCheckError on ADB failure."""
        with self.assertRaises(errors.HealthCheckError):
            self.ad_obj.health_check()

        mock_adb_check_connection.assert_called_once()
        self.ad_obj.__dict__.pop("adb", None)

    @mock.patch.object(
        fuchsia_device.FuchsiaDevice,
        "on_device_boot",
        autospec=True,
    )
    @mock.patch.object(
        adb_transport.Adb,
        "on_device_boot",
        autospec=True,
    )
    @mock.patch.object(
        adb_transport.Adb,
        "check_connection",
        autospec=True,
    )
    @mock.patch.object(
        adb_transport.Adb,
        "verify_supported",
        autospec=True,
    )
    async def test_on_device_boot(
        self,
        mock_adb_verify_supported: mock.Mock,
        mock_adb_check_connection: mock.Mock,
        mock_adb_on_device_boot: mock.Mock,
        mock_super_on_device_boot: mock.Mock,
    ) -> None:
        """Testcase for AndroidDevice.on_device_boot()."""
        _ = self.ad_obj.adb
        await self.ad_obj.on_device_boot()
        mock_adb_on_device_boot.assert_called_once_with(self.ad_obj.adb)
        mock_super_on_device_boot.assert_called_once_with(self.ad_obj)
        self.ad_obj.__dict__.pop("adb", None)

    @mock.patch.object(
        fuchsia_device.FuchsiaDevice,
        "on_device_boot",
        autospec=True,
    )
    @mock.patch.object(
        adb_transport.Adb,
        "verify_supported",
        side_effect=errors.NotSupportedError("ADB not supported"),
        autospec=True,
    )
    async def test_on_device_boot_adb_not_supported(
        self,
        mock_adb_verify_supported: mock.Mock,
        mock_super_on_device_boot: mock.Mock,
    ) -> None:
        """Testcase for AndroidDevice.on_device_boot() raising NotSupportedError when ADB is not supported."""
        with self.assertRaises(errors.NotSupportedError):
            await self.ad_obj.on_device_boot()
        mock_adb_verify_supported.assert_called_once()
        mock_super_on_device_boot.assert_not_called()
        self.ad_obj.__dict__.pop("adb", None)

    @mock.patch.object(
        fuchsia_device.FuchsiaDevice,
        "on_device_boot",
        autospec=True,
    )
    async def test_on_device_boot_adb_disabled(
        self,
        mock_super_on_device_boot: mock.Mock,
    ) -> None:
        """Testcase for AndroidDevice.on_device_boot() when ADB is disabled in config."""
        orig_config = self.ad_obj._config
        self.ad_obj._config = {
            "transports": {
                "adb": {
                    "enabled": False,
                }
            }
        }
        self.ad_obj.__dict__.pop("adb", None)
        try:
            await self.ad_obj.on_device_boot()
            mock_super_on_device_boot.assert_called_once_with(self.ad_obj)
        finally:
            self.ad_obj._config = orig_config
            self.ad_obj.__dict__.pop("adb", None)


if __name__ == "__main__":
    unittest.main()
