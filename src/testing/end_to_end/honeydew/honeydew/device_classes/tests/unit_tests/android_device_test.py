# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for honeydew.device_classes.android_device.py."""

import asyncio
import dataclasses
import ipaddress
import shutil
import unittest
from datetime import timedelta
from pathlib import Path
from typing import Any
from unittest import mock

from honeydew import errors
from honeydew.affordances.android_diagnostics import (
    android_diagnostics as android_diagnostics_affordance,
)
from honeydew.affordances.android_idle import (
    android_idle as android_idle_affordance,
)
from honeydew.affordances.android_package import (
    android_package as android_package_affordance,
)
from honeydew.affordances.android_session import (
    android_session as android_session_affordance,
)
from honeydew.affordances.input import input as input_affordance
from honeydew.affordances.ui.screenshot import (
    screenshot as screenshot_affordance,
)
from honeydew.affordances.ui.user_input import types as ui_types
from honeydew.affordances.ui.user_input import (
    user_input as user_input_affordance,
)
from honeydew.affordances.ui_navigation import (
    ui_navigation as ui_navigation_affordance,
)
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
from honeydew.typing import custom_types
from honeydew.utils import common, host_shell
from mobly import signals
from mobly.controllers import android_device as mobly_android_device

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


class TestAndroidDevice(android_device.AndroidDevice):
    """Concrete test subclass with device-specific defaults configured."""

    sysui_package = "com.test.sysui"
    _default_screen_size = ui_types.Size(600, 800)


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

        ffx_monitor_patcher = mock.patch.object(
            ffx.FFX,
            "_check_running_monitor",
            return_value=False,
            autospec=True,
        )
        ffx_monitor_patcher.start()
        self.addCleanup(ffx_monitor_patcher.stop)

        pontis_which_patcher = mock.patch.object(
            shutil,
            "which",
            return_value="/usr/bin/pontis",
            autospec=True,
        )
        pontis_which_patcher.start()
        self.addCleanup(pontis_which_patcher.stop)

        with (
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
                    address=_INPUT_ARGS["device_ip"],
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
                    address=_INPUT_ARGS["device_ip"],
                    serial_socket=_INPUT_ARGS["device_serial_socket"],
                ),
                ffx_config_data=_INPUT_ARGS["ffx_config_data"],
                config=config,
            )
            with (
                mock.patch.object(
                    ad_obj,
                    "_is_pontis_target",
                    autospec=True,
                ) as mock_is_pontis,
                self.assertRaises(errors.NotEnabledError),
            ):
                _ = ad_obj.adb
            mock_is_pontis.assert_not_called()

    def test_is_pontis_target_device_info_address(self) -> None:
        """Test case for _is_pontis_target() using device_info.address."""
        orig_device_info = self.ad_obj._device_info
        try:
            self.ad_obj._device_info = custom_types.DeviceInfo(
                name=_INPUT_ARGS["device_name"],
                serial_number="12345678",
                address=custom_types.IpPort(
                    ip=ipaddress.IPv6Address(
                        "fe80::bcd3:bbce:3ee9:4d87%pontis"
                    ),
                    port=8022,
                ),
                serial_socket=_INPUT_ARGS["device_serial_socket"],
            )
            self.assertTrue(self.ad_obj._is_pontis_target())

            self.ad_obj._device_info = custom_types.DeviceInfo(
                name=_INPUT_ARGS["device_name"],
                serial_number="12345678",
                address=custom_types.IpPort(
                    ip=ipaddress.IPv4Address("192.168.1.2"),
                    port=8022,
                ),
                serial_socket=_INPUT_ARGS["device_serial_socket"],
            )
            self.assertFalse(self.ad_obj._is_pontis_target())
        finally:
            self.ad_obj._device_info = orig_device_info

    def test_is_pontis_target_ffx_fallback(self) -> None:
        """Test case for _is_pontis_target() falling back to FFX get_target_address."""
        orig_device_info = self.ad_obj._device_info
        self.ad_obj._device_info = custom_types.DeviceInfo(
            name=_INPUT_ARGS["device_name"],
            serial_number="12345678",
            address=None,
            serial_socket=_INPUT_ARGS["device_serial_socket"],
        )
        try:
            with mock.patch.object(
                ffx.FFX,
                "get_target_address",
                return_value=custom_types.IpPort(
                    ip=ipaddress.IPv6Address(
                        "fe80::bcd3:bbce:3ee9:4d87%pontis"
                    ),
                    port=None,
                ),
                autospec=True,
            ):
                self.assertTrue(self.ad_obj._is_pontis_target())

            with mock.patch.object(
                ffx.FFX,
                "get_target_address",
                side_effect=ffx_errors.FfxCommandError("FFX error"),
                autospec=True,
            ):
                self.assertFalse(self.ad_obj._is_pontis_target())
        finally:
            self.ad_obj._device_info = orig_device_info

    @mock.patch.object(adb_transport, "Adb", autospec=True)
    def test_adb_transport_on_loopback_target(
        self, mock_adb_cls: mock.Mock
    ) -> None:
        """Test adb auto-detects TCP on loopback targets unless use_adb_usb is set."""
        for addr_str, config, expected_serial in (
            ("[::1]:8022", None, "[::1]:5555"),
            ("127.0.0.1:8022", None, "127.0.0.1:5555"),
            (
                "[::1]:8022",
                {"transports": {"adb": {"use_adb_usb": True}}},
                "12345678",
            ),
            ("[fe80::1%eth0]:8022", None, "12345678"),
            ("192.168.1.2:8022", None, "12345678"),
        ):
            with self.subTest(addr=addr_str, config=config):
                self.ad_obj.__dict__.pop("adb", None)
                mock_adb_cls.reset_mock()
                self.ad_obj._device_info = dataclasses.replace(
                    self.ad_obj._device_info,
                    address=custom_types.IpPort.create_using_ip_and_port(
                        addr_str
                    ),
                )
                self.ad_obj._config = config
                _ = self.ad_obj.adb
                self.assertEqual(
                    mock_adb_cls.call_args.kwargs["serial_number"],
                    expected_serial,
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
    @mock.patch.object(
        host_shell,
        "run",
        return_value=(
            '{"bridgeStatus":[{"deviceStatus":['
            '{"deviceType":"ADB with optimizations",'
            '"localDeviceId":"ADBFuchsia<other_serial>",'
            '"listenPort":39180},'
            '{"deviceType":"ADB with optimizations",'
            '"localDeviceId":"ADBFuchsia<12345678>",'
            '"listenPort":39181}'
            "]}]}"
        ),
        autospec=True,
    )
    def test_adb_transport_on_pontis_uses_pontis_status_port(
        self,
        mock_host_shell_run: mock.Mock,
        mock_verify_supported: mock.Mock,
        mock_check_connection: mock.Mock,
    ) -> None:
        """Test case for AndroidDevice resolving Pontis ADB port via pontis status."""
        orig_device_info = self.ad_obj._device_info
        self.ad_obj._device_info = custom_types.DeviceInfo(
            name=_INPUT_ARGS["device_name"],
            serial_number="12345678",
            address=custom_types.IpPort(
                ip=ipaddress.IPv6Address("fe80::bcd3:bbce:3ee9:4d87%pontis"),
                port=8022,
            ),
            serial_socket=_INPUT_ARGS["device_serial_socket"],
        )
        self.ad_obj.__dict__.pop("adb", None)
        try:
            adb_obj = self.ad_obj.adb
            self.assertEqual(adb_obj.serial_number, "127.0.0.1:39181")
            mock_host_shell_run.assert_called_once_with(
                cmd=["pontis", "status", "--format=json"],
                capture_output=True,
                timeout=5.0,
            )
            mock_verify_supported.assert_called_once()
            mock_check_connection.assert_called_once()
        finally:
            self.ad_obj._device_info = orig_device_info
            self.ad_obj.__dict__.pop("adb", None)

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
        host_shell,
        "run",
        side_effect=errors.HostCmdError("pontis not found"),
        autospec=True,
    )
    def test_adb_transport_on_pontis_falls_back_to_target_ip(
        self,
        mock_host_shell_run: mock.Mock,
        mock_verify_supported: mock.Mock,
        mock_check_connection: mock.Mock,
    ) -> None:
        """Test case for AndroidDevice falling back to [<pontis_ip>]:5555 on Pontis."""
        orig_device_info = self.ad_obj._device_info
        self.ad_obj._device_info = custom_types.DeviceInfo(
            name=_INPUT_ARGS["device_name"],
            serial_number="12345678",
            address=custom_types.IpPort(
                ip=ipaddress.IPv6Address("fe80::bcd3:bbce:3ee9:4d87%pontis"),
                port=8022,
            ),
            serial_socket=_INPUT_ARGS["device_serial_socket"],
        )
        self.ad_obj.__dict__.pop("adb", None)
        try:
            adb_obj = self.ad_obj.adb
            self.assertEqual(
                adb_obj.serial_number,
                "[fe80::bcd3:bbce:3ee9:4d87%pontis]:5555",
            )
            mock_host_shell_run.assert_called_once()
            mock_verify_supported.assert_called_once()
            mock_check_connection.assert_called_once()
        finally:
            self.ad_obj._device_info = orig_device_info
            self.ad_obj.__dict__.pop("adb", None)

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
        host_shell,
        "run",
        autospec=True,
    )
    def test_adb_transport_on_pontis_skips_status_when_pontis_binary_missing(
        self,
        mock_host_shell_run: mock.Mock,
        mock_verify_supported: mock.Mock,
        mock_check_connection: mock.Mock,
    ) -> None:
        """Test AndroidDevice skips pontis status when pontis binary is not in PATH."""
        orig_device_info = self.ad_obj._device_info
        self.ad_obj._device_info = custom_types.DeviceInfo(
            name=_INPUT_ARGS["device_name"],
            serial_number="12345678",
            address=custom_types.IpPort(
                ip=ipaddress.IPv6Address("fe80::bcd3:bbce:3ee9:4d87%pontis"),
                port=8022,
            ),
            serial_socket=_INPUT_ARGS["device_serial_socket"],
        )
        self.ad_obj.__dict__.pop("adb", None)
        try:
            with mock.patch.object(
                shutil, "which", return_value=None
            ) as mock_which:
                adb_obj = self.ad_obj.adb
                mock_which.assert_called_once_with("pontis")
            self.assertEqual(
                adb_obj.serial_number,
                "[fe80::bcd3:bbce:3ee9:4d87%pontis]:5555",
            )
            mock_host_shell_run.assert_not_called()
            mock_verify_supported.assert_called_once()
            mock_check_connection.assert_called_once()
        finally:
            self.ad_obj._device_info = orig_device_info
            self.ad_obj.__dict__.pop("adb", None)

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
        return_value="12345678",
    )
    @mock.patch.object(
        host_shell,
        "run",
        return_value=(
            '{"bridgeStatus":[{"deviceStatus":['
            '{"deviceType":"ADB with optimizations",'
            '"localDeviceId":"ADBFuchsia<12345678>",'
            '"listenPort":39181}'
            "]}]}"
        ),
        autospec=True,
    )
    def test_adb_transport_on_pontis_with_use_adb_usb_false_and_no_serial(
        self,
        mock_host_shell_run: mock.Mock,
        mock_ffx_serial_number: mock.Mock,
        mock_verify_supported: mock.Mock,
        mock_check_connection: mock.Mock,
    ) -> None:
        """Test Pontis resolution runs before use_adb_usb=False fallback when serial_number is None."""
        orig_device_info = self.ad_obj._device_info
        orig_config = self.ad_obj._config
        self.ad_obj._device_info = custom_types.DeviceInfo(
            name=_INPUT_ARGS["device_name"],
            serial_number=None,
            address=custom_types.IpPort(
                ip=ipaddress.IPv6Address("fe80::bcd3:bbce:3ee9:4d87%pontis"),
                port=8022,
            ),
            serial_socket=_INPUT_ARGS["device_serial_socket"],
        )
        self.ad_obj._config = {"transports": {"adb": {"use_adb_usb": False}}}
        self.ad_obj.__dict__.pop("adb", None)
        try:
            adb_obj = self.ad_obj.adb
            self.assertEqual(adb_obj.serial_number, "127.0.0.1:39181")
            mock_ffx_serial_number.assert_called_once()
            mock_host_shell_run.assert_called_once_with(
                cmd=["pontis", "status", "--format=json"],
                capture_output=True,
                timeout=5.0,
            )
            mock_verify_supported.assert_called_once()
            mock_check_connection.assert_called_once()
        finally:
            self.ad_obj._device_info = orig_device_info
            self.ad_obj._config = orig_config
            self.ad_obj.__dict__.pop("adb", None)

    @mock.patch.object(
        host_shell,
        "run",
        side_effect=signals.TestAbortAll("abort"),
        autospec=True,
    )
    def test_resolve_pontis_adb_serial_propagates_unexpected_exceptions(
        self,
        mock_host_shell_run: mock.Mock,
    ) -> None:
        """Test _resolve_pontis_adb_serial propagates unexpected exceptions (e.g. Mobly signals)."""
        pontis_addr = custom_types.IpPort(
            ip=ipaddress.IPv6Address("fe80::bcd3:bbce:3ee9:4d87%pontis"),
            port=8022,
        )
        with self.assertRaises(signals.TestAbortAll):
            self.ad_obj._resolve_pontis_adb_serial(pontis_addr=pontis_addr)
        mock_host_shell_run.assert_called_once()

    @mock.patch.object(
        host_shell,
        "run",
        return_value=(
            '{"bridgeStatus":[{"deviceStatus":['
            '{"deviceType":"ADB with optimizations",'
            '"localDeviceId":"ADBFuchsia<serial_a>",'
            '"listenPort":39180},'
            '{"deviceType":"ADB with optimizations",'
            '"localDeviceId":"ADBFuchsia<serial_b>",'
            '"listenPort":39181}'
            "]}]}"
        ),
        autospec=True,
    )
    def test_resolve_pontis_adb_serial_warns_on_multiple_unmatched_candidates(
        self,
        mock_host_shell_run: mock.Mock,
    ) -> None:
        """Test _resolve_pontis_adb_serial logs warning and falls back when multiple candidates don't match."""
        pontis_addr = custom_types.IpPort(
            ip=ipaddress.IPv6Address("fe80::bcd3:bbce:3ee9:4d87%pontis"),
            port=8022,
        )
        with self.assertLogs(android_device._LOGGER, level="WARNING") as cm:
            resolved = self.ad_obj._resolve_pontis_adb_serial(
                pontis_addr=pontis_addr,
                hardware_serial="unmatched_serial",
            )
        self.assertEqual(resolved, "[fe80::bcd3:bbce:3ee9:4d87%pontis]:5555")
        self.assertTrue(
            any(
                "Multiple Pontis ADB candidates (2) found" in msg
                for msg in cm.output
            )
        )
        mock_host_shell_run.assert_called_once()

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

    @mock.patch.object(adb_transport, "Adb", autospec=True)
    def test_adb_transport_derives_output_dir_from_ffx_logs_dir(
        self, mock_adb_cls: mock.Mock
    ) -> None:
        """Test adb derives output_dir from ffx_config_data.logs_dir."""
        orig_ffx_config_data = self.ad_obj._ffx_config_data
        orig_config = self.ad_obj._config
        try:
            self.ad_obj._ffx_config_data = dataclasses.replace(
                orig_ffx_config_data, logs_dir="/tmp/logs"
            )
            self.ad_obj._config = None

            _ = self.ad_obj.adb

            mock_adb_cls.assert_called_once_with(
                device_name=self.ad_obj.device_name,
                serial_number=self.ad_obj._device_info.serial_number,
                run_isolated_server=True,
                output_dir=Path("/tmp/logs/adb"),
                vendor_keys_path=None,
                ffx_transport=self.ad_obj.ffx,
                attempts=adb_transport._DEFAULT_RUN_ATTEMPTS,
            )
        finally:
            self.ad_obj.__dict__.pop("adb", None)
            self.ad_obj._ffx_config_data = orig_ffx_config_data
            self.ad_obj._config = orig_config

    @mock.patch.object(adb_transport, "Adb", autospec=True)
    def test_adb_transport_derives_output_dir_when_logs_dir_ends_with_ffx(
        self, mock_adb_cls: mock.Mock
    ) -> None:
        """Test adb derives output_dir when logs_dir ends with ffx."""
        orig_ffx_config_data = self.ad_obj._ffx_config_data
        orig_config = self.ad_obj._config
        try:
            self.ad_obj._ffx_config_data = dataclasses.replace(
                orig_ffx_config_data, logs_dir="/tmp/test_run/ffx"
            )
            self.ad_obj._config = None

            _ = self.ad_obj.adb

            mock_adb_cls.assert_called_once_with(
                device_name=self.ad_obj.device_name,
                serial_number=self.ad_obj._device_info.serial_number,
                run_isolated_server=True,
                output_dir=Path("/tmp/test_run/adb"),
                vendor_keys_path=None,
                ffx_transport=self.ad_obj.ffx,
                attempts=adb_transport._DEFAULT_RUN_ATTEMPTS,
            )
        finally:
            self.ad_obj.__dict__.pop("adb", None)
            self.ad_obj._ffx_config_data = orig_ffx_config_data
            self.ad_obj._config = orig_config

    @mock.patch.object(adb_transport, "Adb", autospec=True)
    def test_adb_transport_uses_configured_output_dir(
        self, mock_adb_cls: mock.Mock
    ) -> None:
        """Test adb uses output_dir from adb_config when provided."""
        orig_ffx_config_data = self.ad_obj._ffx_config_data
        orig_config = self.ad_obj._config
        try:
            self.ad_obj._ffx_config_data = dataclasses.replace(
                orig_ffx_config_data, logs_dir="/tmp/logs"
            )
            self.ad_obj._config = {
                "transports": {
                    "adb": {
                        "output_dir": "/custom/adb_logs",
                    }
                }
            }

            _ = self.ad_obj.adb

            mock_adb_cls.assert_called_once_with(
                device_name=self.ad_obj.device_name,
                serial_number=self.ad_obj._device_info.serial_number,
                run_isolated_server=True,
                output_dir="/custom/adb_logs",
                vendor_keys_path=None,
                ffx_transport=self.ad_obj.ffx,
                attempts=adb_transport._DEFAULT_RUN_ATTEMPTS,
            )
        finally:
            self.ad_obj.__dict__.pop("adb", None)
            self.ad_obj._ffx_config_data = orig_ffx_config_data
            self.ad_obj._config = orig_config

    @mock.patch.object(adb_transport, "Adb", autospec=True)
    def test_adb_transport_output_dir_none_when_no_logs_dir(
        self, mock_adb_cls: mock.Mock
    ) -> None:
        """Test adb passes output_dir=None when logs_dir is not set."""
        orig_ffx_config_data = self.ad_obj._ffx_config_data
        orig_config = self.ad_obj._config
        try:
            self.ad_obj._ffx_config_data = dataclasses.replace(
                orig_ffx_config_data, logs_dir=""
            )
            self.ad_obj._config = None

            _ = self.ad_obj.adb

            mock_adb_cls.assert_called_once_with(
                device_name=self.ad_obj.device_name,
                serial_number=self.ad_obj._device_info.serial_number,
                run_isolated_server=True,
                output_dir=None,
                vendor_keys_path=None,
                ffx_transport=self.ad_obj.ffx,
                attempts=adb_transport._DEFAULT_RUN_ATTEMPTS,
            )
        finally:
            self.ad_obj.__dict__.pop("adb", None)
            self.ad_obj._ffx_config_data = orig_ffx_config_data
            self.ad_obj._config = orig_config

    @mock.patch.object(adb_transport, "Adb", autospec=True)
    def test_adb_transport_custom_attempts_config(
        self, mock_adb_cls: mock.Mock
    ) -> None:
        """Test case to verify honeydew_config.transports.adb.attempts is passed to Adb."""
        orig_config = self.ad_obj._config
        try:
            self.ad_obj._config = {"transports": {"adb": {"attempts": 1}}}
            _ = self.ad_obj.adb

            mock_adb_cls.assert_called_once_with(
                device_name=self.ad_obj.device_name,
                serial_number=self.ad_obj._device_info.serial_number,
                run_isolated_server=True,
                output_dir=Path("/tmp/logs/adb"),
                vendor_keys_path=None,
                ffx_transport=self.ad_obj.ffx,
                attempts=1,
            )
        finally:
            self.ad_obj.__dict__.pop("adb", None)
            self.ad_obj._config = orig_config

    # TODO(b/564946370): Remove this test once 'adb root' is stable on USB.
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
    async def test_adb_routes_root_methods_to_su_root(
        self,
        mock_verify_supported: mock.Mock,
        mock_check_connection: mock.Mock,
    ) -> None:
        """Verifies AndroidDevice.adb routes root methods to su_root."""
        with (
            mock.patch.object(
                adb_transport.Adb, "su_root", autospec=True
            ) as mock_su_root,
            mock.patch.object(
                adb_transport.Adb, "su_unroot", autospec=True
            ) as mock_su_unroot,
            mock.patch.object(
                adb_transport.Adb, "use_su_root", autospec=True
            ) as mock_use_su_root,
        ):
            adb_inst = self.ad_obj.adb
            adb_inst.root(timeout=5.0)
            mock_su_root.assert_called_once_with(adb_inst, timeout=5.0)

            adb_inst.unroot(timeout=5.0)
            mock_su_unroot.assert_called_once_with(adb_inst, timeout=5.0)

            with adb_inst.use_adb_root(timeout=5.0):
                mock_use_su_root.assert_called_once_with(adb_inst, timeout=5.0)
            mock_use_su_root.return_value.__enter__.assert_called_once()
            mock_use_su_root.return_value.__exit__.assert_called_once()

            mock_use_su_root.reset_mock()
            async with adb_inst.use_adb_root(timeout=5.0):
                mock_use_su_root.assert_called_once_with(adb_inst, timeout=5.0)
            mock_use_su_root.return_value.__enter__.assert_called_once()
            mock_use_su_root.return_value.__exit__.assert_called_once()

        self.ad_obj.__dict__.pop("adb", None)

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
    def test_disable_background_dexopt_uses_su_root_without_adb_root(
        self,
        mock_verify_supported: mock.Mock,
        mock_check_connection: mock.Mock,
    ) -> None:
        """Verifies _disable_background_dexopt uses su_root even when use_adb_root is unbound."""
        adb_inst = self.ad_obj.adb
        for attr in ("root", "unroot", "use_adb_root"):
            adb_inst.__dict__.pop(attr, None)
        try:
            with (
                mock.patch.object(
                    adb_transport.Adb, "use_adb_root", autospec=True
                ) as mock_use_adb_root,
                mock.patch.object(
                    adb_transport.Adb, "root", autospec=True
                ) as mock_root,
                mock.patch.object(
                    adb_transport.Adb, "unroot", autospec=True
                ) as mock_unroot,
                mock.patch.object(
                    host_shell, "run", return_value="", autospec=True
                ) as mock_host_shell_run,
            ):
                self.ad_obj._disable_background_dexopt()

                mock_use_adb_root.assert_not_called()
                mock_root.assert_not_called()
                mock_unroot.assert_not_called()
                self.assertFalse(adb_inst.is_su_root)
                self.assertEqual(mock_host_shell_run.call_count, 2)
                self.assertEqual(
                    mock_host_shell_run.call_args_list[0].kwargs["cmd"][-6:],
                    [
                        "shell",
                        "su",
                        "root",
                        "setprop",
                        "pm.dexopt.disable_bg_dexopt",
                        "true",
                    ],
                )
                self.assertEqual(
                    mock_host_shell_run.call_args_list[1].kwargs["cmd"][-4:],
                    ["shell", "pm", "bg-dexopt-job", "--disable"],
                )

                # Verify AdbError suppression on unrooted builds restores
                # is_su_root=False and still attempts pm bg-dexopt-job --disable
                # in its separate try/except block.
                mock_host_shell_run.reset_mock()
                mock_host_shell_run.side_effect = [
                    errors.HostCmdError("su: inaccessible or not found"),
                    "",
                ]
                with self.assertLogs(
                    android_device._LOGGER, level="DEBUG"
                ) as log_cm:
                    self.ad_obj._disable_background_dexopt()

                self.assertTrue(
                    any(
                        "Failed to set pm.dexopt.disable_bg_dexopt" in msg
                        for msg in log_cm.output
                    )
                )
                mock_use_adb_root.assert_not_called()
                mock_root.assert_not_called()
                mock_unroot.assert_not_called()
                self.assertFalse(adb_inst.is_su_root)
                self.assertEqual(mock_host_shell_run.call_count, 2)
                self.assertEqual(
                    mock_host_shell_run.call_args_list[1].kwargs["cmd"][-4:],
                    ["shell", "pm", "bg-dexopt-job", "--disable"],
                )
        finally:
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
            address=None,
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
        "get_target_address",
        side_effect=ffx_errors.FfxCommandError("ffx error"),
        autospec=True,
    )
    @mock.patch.object(
        ffx.FFX,
        "serial_number",
        new_callable=mock.PropertyMock,
    )
    def test_adb_transport_use_adb_usb_false_defaults_to_tcp_serial(
        self,
        mock_ffx_serial_number: mock.Mock,
        mock_get_target_address: mock.Mock,
        mock_verify_supported: mock.Mock,
        mock_check_connection: mock.Mock,
    ) -> None:
        """Test AndroidDevice.adb uses default TCP serial when use_adb_usb is False
        without mutating _device_info.serial_number."""
        orig_device_info: custom_types.DeviceInfo = self.ad_obj._device_info
        orig_config = self.ad_obj._config
        self.ad_obj._device_info = custom_types.DeviceInfo(
            name=_INPUT_ARGS["device_name"],
            serial_number=None,
            address=None,
            serial_socket=None,
        )
        self.ad_obj._config = {"transports": {"adb": {"use_adb_usb": False}}}
        self.ad_obj.__dict__.pop("adb", None)

        try:
            adb_inst: adb_transport.Adb = self.ad_obj.adb
            self.assertIsInstance(adb_inst, adb_transport.Adb)
            self.assertEqual(adb_inst.serial_number, "127.0.0.1:5555")
            self.assertIsNone(self.ad_obj._device_info.serial_number)
            mock_get_target_address.assert_called()
            mock_ffx_serial_number.assert_not_called()
            mock_verify_supported.assert_called_once()
            mock_check_connection.assert_called_once()
        finally:
            self.ad_obj.__dict__.pop("adb", None)
            self.ad_obj._device_info = orig_device_info
            self.ad_obj._config = orig_config

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
        "get_target_address",
        autospec=True,
    )
    @mock.patch.object(
        ffx.FFX,
        "serial_number",
        new_callable=mock.PropertyMock,
    )
    def test_adb_transport_use_adb_usb_false_ffx_target_address_and_usb_serial_override(
        self,
        mock_ffx_serial_number: mock.Mock,
        mock_get_target_address: mock.Mock,
        mock_verify_supported: mock.Mock,
        mock_check_connection: mock.Mock,
    ) -> None:
        """Test use_adb_usb=False overrides USB serial via FFX target address."""
        orig_device_info: custom_types.DeviceInfo = self.ad_obj._device_info
        orig_config = self.ad_obj._config
        self.ad_obj._config = {"transports": {"adb": {"use_adb_usb": False}}}

        try:
            mock_get_target_address.return_value = (
                custom_types.IpPort.create_using_ip_and_port("10.0.0.42:8022")
            )
            self.ad_obj.__dict__.pop("adb", None)
            self.ad_obj._device_info = custom_types.DeviceInfo(
                name=_INPUT_ARGS["device_name"],
                serial_number="USB-HW-SERIAL-123",
                address=None,
                serial_socket=None,
            )
            adb_inst: adb_transport.Adb = self.ad_obj.adb
            self.assertEqual(adb_inst.serial_number, "10.0.0.42:5555")
            mock_get_target_address.assert_called()
            mock_ffx_serial_number.assert_not_called()
        finally:
            self.ad_obj.__dict__.pop("adb", None)
            self.ad_obj._device_info = orig_device_info
            self.ad_obj._config = orig_config

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
    )
    def test_adb_transport_use_adb_usb_false_uses_device_info_ip(
        self,
        mock_ffx_serial_number: mock.Mock,
        mock_verify_supported: mock.Mock,
        mock_check_connection: mock.Mock,
    ) -> None:
        """Test AndroidDevice.adb derives TCP serial from device_info.address IP when use_adb_usb is False."""
        orig_device_info: custom_types.DeviceInfo = self.ad_obj._device_info
        orig_config = self.ad_obj._config
        self.ad_obj._config = {"transports": {"adb": {"use_adb_usb": False}}}

        try:
            for addr_str, expected_serial in (
                ("192.168.42.10:8022", "192.168.42.10:5555"),
                ("[fe80::1%eth0]:8022", "[fe80::1%eth0]:5555"),
            ):
                with self.subTest(addr=addr_str):
                    self.ad_obj.__dict__.pop("adb", None)
                    self.ad_obj._device_info = custom_types.DeviceInfo(
                        name=_INPUT_ARGS["device_name"],
                        serial_number=None,
                        address=custom_types.IpPort.create_using_ip_and_port(
                            addr_str
                        ),
                        serial_socket=None,
                    )
                    adb_inst: adb_transport.Adb = self.ad_obj.adb
                    self.assertEqual(adb_inst.serial_number, expected_serial)
                    mock_ffx_serial_number.assert_not_called()
        finally:
            self.ad_obj.__dict__.pop("adb", None)
            self.ad_obj._device_info = orig_device_info
            self.ad_obj._config = orig_config

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
            address=None,
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
            address=None,
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
        side_effect=signals.TestAbortAll("abort"),
    )
    def test_adb_transport_ffx_serial_propagates_unexpected_exceptions(
        self,
        mock_ffx_serial_number: mock.Mock,
    ) -> None:
        """Test accessing "adb" transport propagates unexpected exceptions
        (e.g. Mobly signals) raised while getting serial_number from FFX."""
        device_info: custom_types.DeviceInfo = self.ad_obj._device_info
        self.ad_obj._device_info = custom_types.DeviceInfo(
            name=_INPUT_ARGS["device_name"],
            serial_number=None,
            address=None,
            serial_socket=None,
        )

        with self.assertRaises(signals.TestAbortAll):
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
        fc_transport.FuchsiaController,
        "check_connection",
        autospec=True,
    )
    @mock.patch.object(ffx.FFX, "check_connection", autospec=True)
    def test_health_check_adb_supported(
        self,
        mock_ffx_check_connection: mock.Mock,
        mock_fc_check_connection: mock.Mock,
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
        mock_adb_check_connection.assert_called_once_with(self.ad_obj.adb)

        self.ad_obj.__dict__.pop("adb", None)

    @mock.patch.object(
        adb_transport.Adb,
        "verify_supported",
        side_effect=errors.NotSupportedError("ADB not supported"),
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
        mock_adb_verify_supported: mock.Mock,
    ) -> None:
        """Testcase for AndroidDevice.health_check() raising NotSupportedError when ADB is not supported."""
        with self.assertRaises(errors.NotSupportedError):
            self.ad_obj.health_check()

        mock_ffx_check_connection.assert_called_once_with(self.ad_obj.ffx)
        mock_fc_check_connection.assert_called_once_with(
            self.ad_obj.fuchsia_controller
        )
        mock_adb_verify_supported.assert_called_once()
        self.ad_obj.__dict__.pop("adb", None)

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
        host_shell,
        "run",
        return_value=(
            '{"bridgeStatus":[{"deviceStatus":['
            '{"deviceType":"ADB with optimizations",'
            '"localDeviceId":"ADBFuchsia<12345678>",'
            '"listenPort":39181}'
            "]}]}"
        ),
        autospec=True,
    )
    @mock.patch.object(
        fc_transport.FuchsiaController,
        "check_connection",
        autospec=True,
    )
    @mock.patch.object(ffx.FFX, "check_connection", autospec=True)
    def test_health_check_on_pontis(
        self,
        mock_ffx_check_connection: mock.Mock,
        mock_fc_check_connection: mock.Mock,
        mock_host_shell_run: mock.Mock,
        mock_adb_verify_supported: mock.Mock,
        mock_adb_check_connection: mock.Mock,
    ) -> None:
        """Testcase for AndroidDevice.health_check() when target is connected via Pontis."""
        orig_device_info = self.ad_obj._device_info
        self.ad_obj._device_info = custom_types.DeviceInfo(
            name=_INPUT_ARGS["device_name"],
            serial_number="12345678",
            address=custom_types.IpPort(
                ip=ipaddress.IPv6Address("fe80::bcd3:bbce:3ee9:4d87%pontis"),
                port=8022,
            ),
            serial_socket=_INPUT_ARGS["device_serial_socket"],
        )
        self.ad_obj.__dict__.pop("adb", None)
        try:
            self.ad_obj.health_check()

            mock_ffx_check_connection.assert_called_once_with(self.ad_obj.ffx)
            mock_fc_check_connection.assert_called_once_with(
                self.ad_obj.fuchsia_controller
            )
            mock_host_shell_run.assert_called_once()
            mock_adb_verify_supported.assert_called_once()
            mock_adb_check_connection.assert_called()
            self.assertEqual(self.ad_obj.adb.serial_number, "127.0.0.1:39181")
        finally:
            self.ad_obj._device_info = orig_device_info
            self.ad_obj.__dict__.pop("adb", None)

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
            self.assertEqual(
                self.ad_obj._health_check_leg_statuses[-1], "adb=skipped"
            )
            self.assertIsNone(self.ad_obj._current_health_check_leg)
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
        fc_transport.FuchsiaController,
        "check_connection",
        autospec=True,
    )
    @mock.patch.object(ffx.FFX, "check_connection", autospec=True)
    def test_health_check_adb_connection_error(
        self,
        mock_ffx_check_connection: mock.Mock,
        mock_fc_check_connection: mock.Mock,
        mock_adb_verify_supported: mock.Mock,
        mock_adb_check_connection: mock.Mock,
    ) -> None:
        """Testcase for AndroidDevice.health_check() raising HealthCheckError on ADB failure."""
        with self.assertRaises(errors.HealthCheckError) as context:
            self.ad_obj.health_check()

        err_msg = str(context.exception)
        self.assertTrue(
            err_msg.startswith(
                f"health check failed on '{self.ad_obj.device_name}': "
            )
        )
        self.assertRegex(
            err_msg,
            r"\[leg=adb \| ffx=ok\(\d+\.\ds\) fc=ok\(\d+\.\ds\) adb=FAIL\(\d+\.\ds\)\] AdbConnectionError: ADB connection error$",
        )
        mock_adb_check_connection.assert_called_once()
        self.ad_obj.__dict__.pop("adb", None)

    @mock.patch.object(
        fuchsia_device.FuchsiaDevice,
        "wait_for_offline",
        autospec=True,
    )
    @mock.patch.object(
        adb_transport.Adb,
        "run",
        return_value="",
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
    def test_wait_for_offline_adb_enabled(
        self,
        mock_adb_verify_supported: mock.Mock,
        mock_adb_check_connection: mock.Mock,
        mock_adb_run: mock.Mock,
        mock_super_wait_for_offline: mock.Mock,
    ) -> None:
        """Testcase for AndroidDevice.wait_for_offline() when ADB is enabled."""
        _ = self.ad_obj.adb
        self.ad_obj.wait_for_offline()
        mock_adb_run.assert_called_once_with(
            self.ad_obj.adb,
            ["wait-for-disconnect"],
            timeout=android_device._WAIT_FOR_DISCONNECT_TIMEOUT_SECS,
            attempts=1,
        )
        mock_super_wait_for_offline.assert_called_once_with(self.ad_obj)
        self.ad_obj.__dict__.pop("adb", None)

    @mock.patch.object(
        fuchsia_device.FuchsiaDevice,
        "wait_for_offline",
        autospec=True,
    )
    def test_wait_for_offline_adb_disabled(
        self,
        mock_super_wait_for_offline: mock.Mock,
    ) -> None:
        """Testcase for AndroidDevice.wait_for_offline() when ADB is disabled or not supported."""
        for exc in (
            errors.NotEnabledError("ADB disabled"),
            errors.NotSupportedError("ADB not supported"),
        ):
            with (
                self.subTest(exc=type(exc).__name__),
                mock.patch.object(
                    android_device.AndroidDevice,
                    "adb",
                    new_callable=mock.PropertyMock,
                    side_effect=exc,
                ) as mock_adb,
            ):
                mock_super_wait_for_offline.reset_mock()
                self.ad_obj.wait_for_offline()
                mock_adb.assert_called_once_with()
                mock_super_wait_for_offline.assert_called_once_with(self.ad_obj)

    @mock.patch.object(
        fuchsia_device.FuchsiaDevice,
        "wait_for_offline",
        autospec=True,
    )
    @mock.patch.object(
        adb_transport.Adb,
        "run",
        side_effect=adb_errors.AdbTimeoutError("ADB disconnect timed out"),
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
    def test_wait_for_offline_adb_error(
        self,
        mock_adb_verify_supported: mock.Mock,
        mock_adb_check_connection: mock.Mock,
        mock_adb_run: mock.Mock,
        mock_super_wait_for_offline: mock.Mock,
    ) -> None:
        """Testcase for AndroidDevice.wait_for_offline() logging warning on AdbError and calling super()."""
        _ = self.ad_obj.adb
        with self.assertLogs(android_device._LOGGER, level="WARNING"):
            self.ad_obj.wait_for_offline()
        mock_adb_run.assert_called_once_with(
            self.ad_obj.adb,
            ["wait-for-disconnect"],
            timeout=android_device._WAIT_FOR_DISCONNECT_TIMEOUT_SECS,
            attempts=1,
        )
        mock_super_wait_for_offline.assert_called_once_with(self.ad_obj)
        self.ad_obj.__dict__.pop("adb", None)

    @mock.patch("time.time", return_value=0.0, autospec=True)
    @mock.patch.object(
        android_device.AndroidDevice,
        "_is_storage_mounted",
        return_value=True,
        autospec=True,
    )
    @mock.patch.object(
        android_device.AndroidDevice,
        "_is_user_unlocked",
        return_value=True,
        autospec=True,
    )
    @mock.patch.object(
        android_device.AndroidDevice,
        "_is_boot_completed",
        return_value=True,
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
    @mock.patch.object(
        fuchsia_device.FuchsiaDevice,
        "wait_for_online",
        autospec=True,
    )
    async def test_wait_for_online(
        self,
        mock_super_wait_for_online: mock.Mock,
        mock_adb_verify_supported: mock.Mock,
        mock_adb_check_connection: mock.Mock,
        mock_is_boot_completed: mock.Mock,
        mock_is_user_unlocked: mock.Mock,
        mock_is_storage_mounted: mock.Mock,
        mock_time: mock.Mock,
    ) -> None:
        """Testcase for AndroidDevice.wait_for_online()."""
        _ = self.ad_obj.adb
        mock_adb_check_connection.reset_mock()
        await self.ad_obj.wait_for_online()
        mock_super_wait_for_online.assert_called_once_with(self.ad_obj)
        mock_adb_check_connection.assert_called_once_with(
            self.ad_obj.adb,
            timeout=android_device._DEFAULT_WAIT_FOR_BOOT_COMPLETE_TIMEOUT_SECS,
        )
        mock_is_boot_completed.assert_called_once_with(self.ad_obj)
        mock_is_user_unlocked.assert_called_once_with(self.ad_obj)
        mock_is_storage_mounted.assert_called_once_with(self.ad_obj)
        self.ad_obj.__dict__.pop("adb", None)

    @mock.patch.object(
        fuchsia_device.FuchsiaDevice,
        "wait_for_online",
        autospec=True,
    )
    async def test_wait_for_online_adb_disabled(
        self,
        mock_super_wait_for_online: mock.Mock,
    ) -> None:
        """Testcase for AndroidDevice.wait_for_online() when ADB is disabled or not supported."""
        for exc in (
            errors.NotEnabledError("ADB disabled"),
            errors.NotSupportedError("ADB not supported"),
        ):
            with (
                self.subTest(exc=type(exc).__name__),
                mock.patch.object(
                    android_device.AndroidDevice,
                    "adb",
                    new_callable=mock.PropertyMock,
                    side_effect=exc,
                ) as mock_adb,
            ):
                mock_super_wait_for_online.reset_mock()
                await self.ad_obj.wait_for_online()
                mock_super_wait_for_online.assert_called_once_with(self.ad_obj)
                mock_adb.assert_called_once_with()

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
        fuchsia_device.FuchsiaDevice,
        "wait_for_online",
        autospec=True,
    )
    async def test_wait_for_online_adb_error(
        self,
        mock_super_wait_for_online: mock.Mock,
        mock_adb_verify_supported: mock.Mock,
        mock_adb_check_connection: mock.Mock,
    ) -> None:
        """Testcase for AndroidDevice.wait_for_online() wrapping AdbError in FuchsiaDeviceError."""
        _ = self.ad_obj.adb
        mock_adb_check_connection.reset_mock()
        mock_adb_check_connection.side_effect = adb_errors.AdbTimeoutError(
            "ADB timed out"
        )
        with self.assertRaises(errors.FuchsiaDeviceError) as ctx:
            await self.ad_obj.wait_for_online()
        self.assertIsInstance(
            ctx.exception.__cause__, adb_errors.AdbTimeoutError
        )
        mock_super_wait_for_online.assert_called_once_with(self.ad_obj)
        mock_adb_check_connection.assert_called_once()
        self.ad_obj.__dict__.pop("adb", None)

    @mock.patch.object(
        android_device.AndroidDevice,
        "_refresh_screen_size",
        autospec=True,
    )
    @mock.patch.object(
        android_session_affordance.AndroidSession,
        "record_session",
        autospec=True,
    )
    @mock.patch.object(
        android_device.AndroidDevice,
        "_enable_stay_on_while_plugged_in",
        autospec=True,
    )
    @mock.patch.object(
        android_device.AndroidDevice,
        "_disable_background_dexopt",
        autospec=True,
    )
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
        mock_disable_dexopt: mock.Mock,
        mock_enable_stay_on: mock.Mock,
        mock_record_session: mock.Mock,
        mock_refresh_screen_size: mock.Mock,
    ) -> None:
        """Testcase for AndroidDevice.on_device_boot()."""
        _ = self.ad_obj.adb
        await self.ad_obj.on_device_boot()
        mock_adb_on_device_boot.assert_called_once_with(self.ad_obj.adb)
        mock_super_on_device_boot.assert_called_once_with(self.ad_obj)
        mock_disable_dexopt.assert_called_once_with(self.ad_obj)
        mock_enable_stay_on.assert_called_once_with(self.ad_obj)
        mock_record_session.assert_called_once_with(self.ad_obj.android_session)
        mock_refresh_screen_size.assert_called_once_with(self.ad_obj)
        self.ad_obj.__dict__.pop("android_session", None)
        self.ad_obj.__dict__.pop("adb", None)

    @mock.patch.object(
        android_device.AndroidDevice,
        "_on_boot_complete",
        autospec=True,
    )
    @mock.patch.object(
        fc_transport.FuchsiaController,
        "check_connection",
        autospec=True,
    )
    @mock.patch.object(
        fc_transport.FuchsiaController,
        "create_context",
        autospec=True,
    )
    @mock.patch.object(
        ffx.FFX,
        "check_connection",
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
    async def test_on_device_boot_runs_adb_health_check(
        self,
        mock_adb_verify_supported: mock.Mock,
        mock_adb_check_connection: mock.Mock,
        mock_adb_on_device_boot: mock.Mock,
        mock_ffx_check_connection: mock.Mock,
        mock_fc_create_context: mock.Mock,
        mock_fc_check_connection: mock.Mock,
        mock_on_boot_complete: mock.Mock,
    ) -> None:
        """Test on_device_boot runs adb.check_connection in health_check."""
        _ = self.ad_obj.adb
        mock_adb_check_connection.reset_mock()
        await self.ad_obj.on_device_boot()
        mock_adb_on_device_boot.assert_called_once_with(self.ad_obj.adb)
        mock_adb_check_connection.assert_called_once_with(self.ad_obj.adb)
        mock_on_boot_complete.assert_called_once_with(self.ad_obj)
        self.ad_obj.__dict__.pop("adb", None)

    @mock.patch.object(
        android_device.AndroidDevice,
        "_on_boot_complete",
        autospec=True,
    )
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
        mock_on_boot_complete: mock.Mock,
    ) -> None:
        """Testcase for AndroidDevice.on_device_boot() skipping ADB when ADB is not supported."""
        await self.ad_obj.on_device_boot()
        mock_adb_verify_supported.assert_called_once()
        mock_super_on_device_boot.assert_called_once_with(self.ad_obj)
        mock_on_boot_complete.assert_not_called()
        self.ad_obj.__dict__.pop("adb", None)

    @mock.patch.object(
        android_device.AndroidDevice,
        "_on_boot_complete",
        autospec=True,
    )
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
    @mock.patch.object(
        host_shell,
        "run",
        side_effect=errors.HostCmdError("pontis not found"),
        autospec=True,
    )
    async def test_on_device_boot_on_pontis(
        self,
        mock_host_shell_run: mock.Mock,
        mock_adb_verify_supported: mock.Mock,
        mock_adb_check_connection: mock.Mock,
        mock_adb_on_device_boot: mock.Mock,
        mock_super_on_device_boot: mock.Mock,
        mock_on_boot_complete: mock.Mock,
    ) -> None:
        """Testcase for AndroidDevice.on_device_boot() when target is connected via Pontis."""
        orig_device_info = self.ad_obj._device_info
        self.ad_obj._device_info = custom_types.DeviceInfo(
            name=_INPUT_ARGS["device_name"],
            serial_number="12345678",
            address=custom_types.IpPort(
                ip=ipaddress.IPv6Address("fe80::bcd3:bbce:3ee9:4d87%pontis"),
                port=8022,
            ),
            serial_socket=_INPUT_ARGS["device_serial_socket"],
        )
        self.ad_obj.__dict__.pop("adb", None)
        try:
            await self.ad_obj.on_device_boot()
            mock_host_shell_run.assert_called_once()
            mock_adb_verify_supported.assert_called_once()
            mock_adb_check_connection.assert_called_once()
            mock_adb_on_device_boot.assert_called_once_with(self.ad_obj.adb)
            mock_super_on_device_boot.assert_called_once_with(self.ad_obj)
            mock_on_boot_complete.assert_called_once_with(self.ad_obj)
            self.assertEqual(
                self.ad_obj.adb.serial_number,
                "[fe80::bcd3:bbce:3ee9:4d87%pontis]:5555",
            )
        finally:
            self.ad_obj._device_info = orig_device_info
            self.ad_obj.__dict__.pop("adb", None)

    @mock.patch.object(
        android_device.AndroidDevice,
        "_on_boot_complete",
        autospec=True,
    )
    @mock.patch.object(
        android_session_affordance.AndroidSession,
        "record_session",
        autospec=True,
    )
    @mock.patch.object(
        android_device.AndroidDevice,
        "_enable_stay_on_while_plugged_in",
        autospec=True,
    )
    @mock.patch.object(
        android_device.AndroidDevice,
        "_disable_background_dexopt",
        autospec=True,
    )
    @mock.patch.object(
        fuchsia_device.FuchsiaDevice,
        "on_device_boot",
        autospec=True,
    )
    async def test_on_device_boot_adb_disabled(
        self,
        mock_super_on_device_boot: mock.Mock,
        mock_disable_dexopt: mock.Mock,
        mock_enable_stay_on: mock.Mock,
        mock_record_session: mock.Mock,
        mock_on_boot_complete: mock.Mock,
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
            mock_disable_dexopt.assert_not_called()
            mock_enable_stay_on.assert_not_called()
            mock_record_session.assert_not_called()
            mock_on_boot_complete.assert_not_called()
        finally:
            self.ad_obj._config = orig_config
            self.ad_obj.__dict__.pop("adb", None)


class AndroidDeviceBaseCapabilitiesTest(unittest.IsolatedAsyncioTestCase):
    """Unit tests for base AndroidDevice profile, geometry, and diagnostics."""

    def setUp(self) -> None:
        super().setUp()
        self.dev_info = custom_types.DeviceInfo(
            name="fuchsia-emulator",
            serial_number=None,
            address=None,
            serial_socket=None,
        )
        self.ffx_cfg = mock.create_autospec(
            ffx_config.FfxConfigData, instance=True
        )
        self.mock_ffx = mock.create_autospec(ffx.FFX, instance=True)
        self.mock_screenshot = mock.create_autospec(
            screenshot_affordance.Screenshot, instance=True
        )
        self.mock_adb: Any = mock.create_autospec(
            adb_transport.Adb, instance=True
        )
        self.mock_adb.run.return_value = ""

        for patcher in (
            mock.patch.object(
                fuchsia_device.FuchsiaDevice, "health_check", autospec=True
            ),
            mock.patch.object(
                fuchsia_device.FuchsiaDevice,
                "wait_for_online",
                autospec=True,
            ),
            mock.patch.object(
                fuchsia_device.FuchsiaDevice,
                "ffx",
                new_callable=mock.PropertyMock,
                return_value=self.mock_ffx,
            ),
            mock.patch.object(
                fuchsia_device.FuchsiaDevice,
                "screenshot",
                new_callable=mock.PropertyMock,
                return_value=self.mock_screenshot,
            ),
            mock.patch.object(
                android_device.AndroidDevice,
                "adb",
                new_callable=mock.PropertyMock,
                return_value=self.mock_adb,
            ),
        ):
            patcher.start()
            self.addCleanup(patcher.stop)

        self.device = TestAndroidDevice(
            device_info=self.dev_info,
            ffx_config_data=self.ffx_cfg,
        )

    def test_init_via_super_fuchsia_device(self) -> None:
        """Verifies initializing AndroidDevice via FuchsiaDevice.__init__."""
        self.assertEqual(self.device.device_name, "fuchsia-emulator")
        self.assertIsNone(self.device._device_info.serial_number)
        self.assertIsNone(self.device._screen_size_override)

    def test_init_use_adb_usb_config(self) -> None:
        """Verifies use_adb_usb=False in config does not mutate DeviceInfo.serial_number."""
        dev_info = custom_types.DeviceInfo(
            name="fuchsia-device",
            serial_number=None,
            address=None,
            serial_socket=None,
        )
        dev = TestAndroidDevice(
            device_info=dev_info,
            ffx_config_data=self.ffx_cfg,
            config={"transports": {"adb": {"use_adb_usb": True}}},
        )
        self.assertIsNone(dev._device_info.serial_number)

        dev_tcp = TestAndroidDevice(
            device_info=dev_info,
            ffx_config_data=self.ffx_cfg,
            config={"transports": {"adb": {"use_adb_usb": False}}},
        )
        self.assertIsNone(dev_tcp._device_info.serial_number)

    def test_initialize_called_in_init_for_product_subclass(self) -> None:
        """Verifies __init__ calls initialize() and _product_setup()."""
        with mock.patch.object(
            TestAndroidDevice, "_product_setup", autospec=True
        ) as mock_prod_setup:
            dev = TestAndroidDevice(
                device_info=self.dev_info,
                ffx_config_data=self.ffx_cfg,
                config={"screen_size": "640x480"},
            )
            self.assertEqual(dev._screen_size_override, ui_types.Size(640, 480))
            self.assertEqual(dev.screen_size(), ui_types.Size(640, 480))
            mock_prod_setup.assert_called_once_with(dev)

    def test_initialize_skips_without_product_profile(self) -> None:
        """Verifies initialize() is a no-op when sysui_package is empty."""
        with mock.patch.object(
            android_device.AndroidDevice, "_product_setup", autospec=True
        ) as mock_prod_setup:
            bare_device = android_device.AndroidDevice(
                device_info=self.dev_info,
                ffx_config_data=self.ffx_cfg,
            )
            bare_device.initialize()
            mock_prod_setup.assert_not_called()

    def test_initialize_skips_when_adb_disabled(self) -> None:
        """Verifies initialize() skips ADB calls when ADB is disabled or not supported."""
        for exc in (
            errors.NotEnabledError("adb disabled"),
            errors.NotSupportedError("adb not supported"),
        ):
            with (
                self.subTest(exc=type(exc).__name__),
                mock.patch.object(
                    TestAndroidDevice,
                    "screen_size",
                    side_effect=exc,
                    autospec=True,
                ),
                mock.patch.object(
                    TestAndroidDevice, "_product_setup", autospec=True
                ) as mock_prod_setup,
            ):
                dev = TestAndroidDevice(
                    device_info=self.dev_info,
                    ffx_config_data=self.ffx_cfg,
                )
                dev.initialize()
                mock_prod_setup.assert_not_called()

    def test_initialize_propagates_not_enabled_error_from_product_setup(
        self,
    ) -> None:
        """Verifies NotEnabledError raised inside _product_setup is not swallowed."""
        with (
            mock.patch.object(
                TestAndroidDevice,
                "screen_size",
                return_value=ui_types.Size(600, 800),
                autospec=True,
            ),
            mock.patch.object(
                TestAndroidDevice,
                "_product_setup",
                side_effect=errors.NotEnabledError("affordance disabled"),
                autospec=True,
            ),
        ):
            with self.assertRaises(errors.NotEnabledError):
                TestAndroidDevice(
                    device_info=self.dev_info,
                    ffx_config_data=self.ffx_cfg,
                )

    def test_screen_size_default_and_config(self) -> None:
        """Verifies screen_size() default and config parsing/validation."""
        self.assertEqual(self.device.screen_size(), ui_types.Size(600, 800))
        dev = TestAndroidDevice(
            device_info=self.dev_info,
            ffx_config_data=self.ffx_cfg,
            config={"screen_size": "1234x5678"},
        )
        self.assertEqual(dev.screen_size(), ui_types.Size(1234, 5678))

        with self.assertRaises(errors.ConfigError):
            TestAndroidDevice(
                device_info=self.dev_info,
                ffx_config_data=self.ffx_cfg,
                config={"screen_size": "invalid_geometry"},
            )

        bare_device = android_device.AndroidDevice(
            device_info=self.dev_info,
            ffx_config_data=self.ffx_cfg,
        )
        with self.assertRaises(errors.ConfigError):
            bare_device.screen_size()

    def test_screen_size_query_and_precedence(self) -> None:
        """Verifies wm size Physical/Override query and fallback."""
        self.mock_adb.run.return_value = (
            "Physical size: 500x1000\nOverride size: 400x800"
        )
        self.assertEqual(self.device.screen_size(), ui_types.Size(400, 800))
        self.assertEqual(
            self.device._screen_size_cache, ui_types.Size(400, 800)
        )

    def test_screen_size_error_fallback(self) -> None:
        """Verifies screen_size falls back on ADB failure."""
        self.mock_adb.run.side_effect = adb_errors.AdbCommandError(
            "ADB query failure"
        )
        with self.assertLogs(level="WARNING") as log:
            size = self.device.screen_size()
        self.assertEqual(size, ui_types.Size(600, 800))
        self.assertIn("Failed to query screen size via wm size", log.output[0])

    def test_screen_size_propagates_not_enabled_error(self) -> None:
        """Verifies screen_size propagates NotEnabledError when ADB is disabled."""
        with (
            mock.patch.object(
                android_device.AndroidDevice,
                "adb",
                new_callable=mock.PropertyMock,
                side_effect=errors.NotEnabledError("ADB disabled"),
            ),
            self.assertRaises(errors.NotEnabledError),
        ):
            self.device.screen_size()

    def test_android_session_affordance(self) -> None:
        """Verifies android_session returns a cached AndroidSession."""
        session: Any = self.device.android_session
        self.assertIsInstance(
            session, android_session_affordance.AndroidSession
        )
        self.assertIs(self.device.android_session, session)
        self.assertIs(session._adb, self.mock_adb)

    def test_diagnostics_affordance(self) -> None:
        """Verifies diagnostics returns a cached AndroidDiagnostics."""
        diagnostics: Any = self.device.diagnostics
        self.assertIsInstance(
            diagnostics,
            android_diagnostics_affordance.AndroidDiagnostics,
        )
        self.assertIs(self.device.diagnostics, diagnostics)
        self.assertIs(diagnostics._adb, self.mock_adb)

    def test_android_package_affordance(self) -> None:
        """Verifies android_package returns a cached AndroidPackage."""
        package: Any = self.device.android_package
        self.assertIsInstance(
            package, android_package_affordance.AndroidPackage
        )
        self.assertIs(self.device.android_package, package)
        self.assertIs(package._adb, self.mock_adb)

    def test_android_idle_affordance_and_config(self) -> None:
        """Verifies android_idle returns a cached AndroidIdle, registers close callback, and respects config."""
        with mock.patch.object(
            self.device, "register_for_on_device_close", autospec=True
        ) as mock_register_close:
            idle: Any = self.device.android_idle
            mock_register_close.assert_called_once_with(idle.close)
        self.assertIsInstance(idle, android_idle_affordance.AndroidIdle)
        self.assertIs(self.device.android_idle, idle)
        self.assertIs(idle._adb, self.mock_adb)
        self.assertEqual(
            self.device._resolve_after_boot_duration(), timedelta(seconds=0)
        )
        self.assertEqual(self.device._resolve_idled_after_boot_prop(), "")

        configured = TestAndroidDevice(
            device_info=self.dev_info,
            ffx_config_data=self.ffx_cfg,
            config={
                "affordances": {
                    "android_idle": {
                        "after_boot_duration_seconds": 45,
                        "idled_after_boot_prop": "debug.custom.idled",
                    }
                }
            },
        )
        self.assertEqual(
            configured._resolve_after_boot_duration(), timedelta(seconds=45)
        )
        self.assertEqual(
            configured._resolve_idled_after_boot_prop(), "debug.custom.idled"
        )

    def test_on_boot_complete_refreshes_screen_size_and_runs_setup(
        self,
    ) -> None:
        """Verifies _on_boot_complete runs post-boot setup, records session, and refreshes screen size."""
        with (
            mock.patch.object(
                self.device, "_disable_background_dexopt", autospec=True
            ) as mock_disable_dexopt,
            mock.patch.object(
                self.device, "_enable_stay_on_while_plugged_in", autospec=True
            ) as mock_enable_stay_on,
            mock.patch.object(
                self.device, "android_session", autospec=True
            ) as mock_session,
            mock.patch.object(
                self.device, "screen_size", autospec=True
            ) as mock_screen_size,
            mock.patch.object(
                self.device, "_product_setup", autospec=True
            ) as mock_product_setup,
            mock.patch.object(
                self.device, "dismiss_blocking_dialogs", autospec=True
            ) as mock_dismiss,
        ):
            self.device._on_boot_complete()
        mock_disable_dexopt.assert_called_once_with()
        mock_enable_stay_on.assert_called_once_with()
        mock_session.record_session.assert_called_once_with()
        mock_screen_size.assert_called_once_with()
        mock_product_setup.assert_called_once_with()
        mock_dismiss.assert_called_once_with()

        # Verify configure_on_boot=False skips _disable_background_dexopt and _enable_stay_on_while_plugged_in
        orig_config = self.device._config
        self.device._config = {"configure_on_boot": False}
        try:
            with (
                mock.patch.object(
                    self.device, "_disable_background_dexopt", autospec=True
                ) as mock_disable_dexopt,
                mock.patch.object(
                    self.device,
                    "_enable_stay_on_while_plugged_in",
                    autospec=True,
                ) as mock_enable_stay_on,
                mock.patch.object(
                    self.device, "android_session", autospec=True
                ) as mock_session,
                mock.patch.object(
                    self.device, "screen_size", autospec=True
                ) as mock_screen_size,
                mock.patch.object(
                    self.device, "_product_setup", autospec=True
                ) as mock_product_setup,
                mock.patch.object(
                    self.device, "dismiss_blocking_dialogs", autospec=True
                ) as mock_dismiss,
            ):
                self.device._on_boot_complete()
            mock_disable_dexopt.assert_not_called()
            mock_enable_stay_on.assert_not_called()
            mock_session.record_session.assert_called_once_with()
            mock_screen_size.assert_called_once_with()
            mock_product_setup.assert_called_once_with()
            mock_dismiss.assert_called_once_with()
        finally:
            self.device._config = orig_config

        for exc in (
            errors.ConfigError("no screen size"),
            errors.NotEnabledError("adb disabled"),
            errors.NotSupportedError("adb not supported"),
        ):
            with (
                mock.patch.object(
                    self.device, "_disable_background_dexopt", autospec=True
                ),
                mock.patch.object(
                    self.device,
                    "_enable_stay_on_while_plugged_in",
                    autospec=True,
                ),
                mock.patch.object(
                    self.device, "android_session", autospec=True
                ),
                mock.patch.object(
                    self.device,
                    "screen_size",
                    side_effect=exc,
                    autospec=True,
                ),
                mock.patch.object(self.device, "_product_setup", autospec=True),
                mock.patch.object(
                    self.device, "dismiss_blocking_dialogs", autospec=True
                ),
                self.assertLogs(level="DEBUG") as log,
            ):
                self.device._on_boot_complete()
            self.assertTrue(
                any("Skipping screen size refresh" in m for m in log.output)
            )

    async def test_reboot_waits_for_android_boot(self) -> None:
        """Verifies reboot() announces session change and delegates to super().reboot()."""
        manager = mock.Mock()
        with (
            mock.patch.object(
                fuchsia_device.FuchsiaDevice, "reboot", autospec=True
            ) as mock_super_reboot,
            mock.patch.object(
                self.device, "android_session", autospec=True
            ) as mock_session,
        ):
            manager.attach_mock(
                mock_session.session_will_change, "session_will_change"
            )
            manager.attach_mock(mock_super_reboot, "super_reboot")
            await self.device.reboot()
        self.assertEqual(
            [c[0] for c in manager.mock_calls],
            [
                "session_will_change",
                "super_reboot",
            ],
        )

    async def test_reboot_adb_disabled(self) -> None:
        """Verifies reboot() skips session_will_change and calls super().reboot() when ADB is disabled or not supported."""
        for exc in (
            errors.NotEnabledError("ADB disabled"),
            errors.NotSupportedError("ADB not supported"),
        ):
            with (
                self.subTest(exc=type(exc).__name__),
                mock.patch.object(
                    fuchsia_device.FuchsiaDevice,
                    "reboot",
                    autospec=True,
                ) as mock_super_reboot,
                mock.patch.object(
                    android_device.AndroidDevice,
                    "android_session",
                    new_callable=mock.PropertyMock,
                    side_effect=exc,
                ),
            ):
                await self.device.reboot()
            mock_super_reboot.assert_awaited_once_with(self.device)

    @mock.patch("time.time", return_value=0.0, autospec=True)
    async def test_wait_for_online_success_immediately(
        self, mock_time: mock.Mock
    ) -> None:
        """Verifies wait_for_online succeeds when all readiness checks pass."""
        self.mock_adb.reset_mock()
        self.mock_adb.getprop.side_effect = ["1", "true"]
        self.mock_adb.run.side_effect = [
            "100 90 0:25 / /storage/emulated rw,nosuid,nodev\n",
        ]
        await self.device.wait_for_online()

        self.mock_adb.check_connection.assert_called_once_with(
            timeout=android_device._DEFAULT_WAIT_FOR_BOOT_COMPLETE_TIMEOUT_SECS
        )
        self.mock_adb.getprop.assert_has_calls(
            [
                mock.call(
                    "sys.boot_completed",
                    timeout=android_device._BOOT_COMPLETED_GETPROP_TIMEOUT_SECS,
                    attempts=1,
                ),
                mock.call(
                    "sys.user.0.ce_available",
                    timeout=android_device._BOOT_COMPLETED_GETPROP_TIMEOUT_SECS,
                    attempts=1,
                ),
            ]
        )
        self.mock_adb.run.assert_called_once_with(
            ["shell", "cat", "/proc/self/mountinfo"],
            timeout=android_device._BOOT_COMPLETED_GETPROP_TIMEOUT_SECS,
            attempts=1,
        )

    @mock.patch("asyncio.sleep", new_callable=mock.AsyncMock)
    async def test_wait_for_online_success_after_polling(
        self, mock_sleep: mock.AsyncMock
    ) -> None:
        """Verifies wait_for_online polls across all readiness steps until each succeeds."""
        self.mock_adb.reset_mock()
        self.mock_adb.getprop.side_effect = ["0", "", "1", "false", "false"]
        self.mock_adb.run.side_effect = [
            "State: RUNNING_LOCKED\n",
            "mUserUnlocked=true\n",
            "80 1 0:20 / /system ro,relatime\n",
            "100 90 0:25 / /data rw,nosuid,nodev\n",
        ]
        await self.device.wait_for_online(poll_interval=0.5)

        self.assertEqual(self.mock_adb.getprop.call_count, 5)
        self.assertEqual(mock_sleep.await_count, 4)
        mock_sleep.assert_awaited_with(0.5)

    def test_is_boot_completed(self) -> None:
        """Verifies _is_boot_completed returns True only when sys.boot_completed == '1'."""
        self.mock_adb.getprop.return_value = "1"
        self.assertTrue(self.device._is_boot_completed())

        self.mock_adb.getprop.return_value = "0"
        self.assertFalse(self.device._is_boot_completed())

        self.mock_adb.getprop.side_effect = adb_errors.AdbCommandError(
            "getprop failed"
        )
        self.assertFalse(self.device._is_boot_completed())

    def test_is_user_unlocked(self) -> None:
        """Verifies _is_user_unlocked checks sys.user.0.ce_available first, then dumpsys user."""
        # 1. Fast path: sys.user.0.ce_available == 'true'
        self.mock_adb.reset_mock()
        self.mock_adb.getprop.return_value = "true"
        self.assertTrue(self.device._is_user_unlocked())
        self.mock_adb.run.assert_not_called()

        # 2. Fallback: mUserUnlocked=true in dumpsys user
        self.mock_adb.reset_mock()
        self.mock_adb.getprop.return_value = "false"
        self.mock_adb.run.return_value = "UserInfo{0} mUserUnlocked=true\n"
        self.assertTrue(self.device._is_user_unlocked())

        # 3. Fallback: State: RUNNING_UNLOCKED in dumpsys user
        self.mock_adb.reset_mock()
        self.mock_adb.getprop.return_value = ""
        self.mock_adb.run.return_value = "State: RUNNING_UNLOCKED\n"
        self.assertTrue(self.device._is_user_unlocked())

        # 4. Locked user returns False
        self.mock_adb.reset_mock()
        self.mock_adb.getprop.return_value = "false"
        self.mock_adb.run.return_value = "State: RUNNING_LOCKED\n"
        self.assertFalse(self.device._is_user_unlocked())

        # 5. AdbError returns False
        self.mock_adb.reset_mock()
        self.mock_adb.getprop.side_effect = adb_errors.AdbCommandError(
            "adb error"
        )
        self.assertFalse(self.device._is_user_unlocked())

    def test_is_storage_mounted(self) -> None:
        """Verifies _is_storage_mounted checks /proc/self/mountinfo for /storage/emulated or /data."""
        for out in (
            "",
            "80 1 0:20 / /system ro,relatime\n",
            "81 1 0:21 / /vendor ro,relatime\n",
        ):
            with self.subTest(out=out):
                self.mock_adb.run.return_value = out
                self.assertFalse(self.device._is_storage_mounted())

        for out in (
            "100 90 0:25 / /storage/emulated rw,nosuid,nodev\n",
            "101 90 0:25 / /data rw,nosuid,nodev\n",
            "102 90 0:25 /media/0 /data/media/0 rw,nosuid,nodev\n",
        ):
            with self.subTest(out=out):
                self.mock_adb.run.return_value = out
                self.assertTrue(self.device._is_storage_mounted())

        self.mock_adb.run.side_effect = adb_errors.AdbCommandError(
            "cat /proc/self/mountinfo failed"
        )
        self.assertFalse(self.device._is_storage_mounted())

    @mock.patch("asyncio.sleep", new_callable=mock.AsyncMock)
    async def test_wait_for_online_success_after_adb_errors(
        self, mock_sleep: mock.AsyncMock
    ) -> None:
        """Verifies wait_for_online ignores AdbErrors during polling and succeeds."""
        self.mock_adb.reset_mock()
        self.mock_adb.getprop.side_effect = [
            adb_errors.AdbCommandError("device offline"),
            "0",
            "1",
            "false",
            "false",
        ]
        self.mock_adb.run.side_effect = [
            adb_errors.AdbCommandError("dumpsys user failed"),
            "mUserUnlocked=true\n",
            adb_errors.AdbCommandError("cat /proc/self/mountinfo failed"),
            "100 90 0:25 / /data rw,nosuid,nodev\n",
        ]

        await self.device.wait_for_online(poll_interval=0.5)

        self.assertEqual(self.mock_adb.getprop.call_count, 5)
        self.assertEqual(mock_sleep.await_count, 4)

    @mock.patch("asyncio.sleep", new_callable=mock.AsyncMock)
    @mock.patch("time.time", autospec=True)
    async def test_wait_for_online_timeout(
        self,
        mock_time: mock.Mock,
        mock_sleep: mock.AsyncMock,
    ) -> None:
        """Verifies wait_for_online raises FuchsiaDeviceError when timeout expires."""
        self.mock_adb.reset_mock()
        mock_time.side_effect = [0.0, 0.0, 0.0, 0.0, 1.0, 10.0]
        self.mock_adb.getprop.return_value = "0"

        with self.assertRaises(errors.FuchsiaDeviceError):
            await self.device.wait_for_online(timeout=5.0)

        self.mock_adb.getprop.assert_called_once_with(
            "sys.boot_completed",
            timeout=5.0,
            attempts=1,
        )
        mock_sleep.assert_awaited_once_with(1.0)

    @mock.patch("time.time", autospec=True)
    async def test_wait_for_online_remaining_timeout_exhausted(
        self,
        mock_time: mock.Mock,
    ) -> None:
        """Verifies wait_for_online raises FuchsiaDeviceError immediately if remaining_timeout <= 0."""
        # Case 1: timeout exhausted during super().wait_for_online() before adb.check_connection()
        self.mock_adb.reset_mock()
        mock_time.side_effect = [0.0, 5.0]

        with self.assertRaises(errors.FuchsiaDeviceError):
            await self.device.wait_for_online(timeout=5.0)

        self.mock_adb.check_connection.assert_not_called()
        self.mock_adb.getprop.assert_not_called()

        # Case 2: timeout exhausted during adb.check_connection() before readiness step loop
        self.mock_adb.reset_mock()
        mock_time.side_effect = [0.0, 1.0, 5.0]

        with self.assertRaises(errors.FuchsiaDeviceError):
            await self.device.wait_for_online(timeout=5.0)

        self.mock_adb.check_connection.assert_called_once_with(timeout=4.0)
        self.mock_adb.getprop.assert_not_called()

    def test_disable_background_dexopt_success(self) -> None:
        """Verifies _disable_background_dexopt sets sysprop under su_root and disables bg-dexopt-job."""
        self.mock_adb.reset_mock()
        self.device._disable_background_dexopt()
        self.mock_adb.use_su_root.assert_called_once_with(
            timeout=android_device._BOOT_SETUP_CMD_TIMEOUT_SECS
        )
        self.mock_adb.use_su_root.return_value.__enter__.assert_called_once()
        self.mock_adb.use_su_root.return_value.__exit__.assert_called_once()
        self.mock_adb.use_adb_root.assert_not_called()
        self.mock_adb.setprop.assert_called_once_with(
            "pm.dexopt.disable_bg_dexopt",
            "true",
            timeout=android_device._BOOT_SETUP_CMD_TIMEOUT_SECS,
            attempts=1,
        )
        self.mock_adb.run.assert_called_once_with(
            ["shell", "pm", "bg-dexopt-job", "--disable"],
            timeout=android_device._BOOT_SETUP_CMD_TIMEOUT_SECS,
            attempts=1,
        )

    def test_disable_background_dexopt_continues_on_setprop_error(
        self,
    ) -> None:
        """Verifies _disable_background_dexopt still attempts pm bg-dexopt-job if setprop raises AdbError."""
        self.mock_adb.reset_mock()
        self.mock_adb.setprop.side_effect = adb_errors.AdbCommandError(
            "setprop failed"
        )
        with self.assertLogs(android_device._LOGGER, level="DEBUG") as log_cm:
            self.device._disable_background_dexopt()
        self.assertTrue(
            any(
                "Failed to set pm.dexopt.disable_bg_dexopt" in msg
                for msg in log_cm.output
            )
        )
        self.mock_adb.use_su_root.assert_called_once_with(
            timeout=android_device._BOOT_SETUP_CMD_TIMEOUT_SECS
        )
        self.mock_adb.use_adb_root.assert_not_called()
        self.mock_adb.setprop.assert_called_once()
        self.mock_adb.run.assert_called_once_with(
            ["shell", "pm", "bg-dexopt-job", "--disable"],
            timeout=android_device._BOOT_SETUP_CMD_TIMEOUT_SECS,
            attempts=1,
        )

    def test_disable_background_dexopt_ignores_bg_dexopt_job_error(
        self,
    ) -> None:
        """Verifies _disable_background_dexopt ignores AdbError from pm bg-dexopt-job."""
        self.mock_adb.reset_mock()
        self.mock_adb.run.side_effect = adb_errors.AdbCommandError(
            "bg-dexopt failed"
        )
        with self.assertLogs(android_device._LOGGER, level="DEBUG") as log_cm:
            self.device._disable_background_dexopt()
        self.assertTrue(
            any(
                "Failed to disable background dexopt job" in msg
                for msg in log_cm.output
            )
        )
        self.mock_adb.use_su_root.assert_called_once_with(
            timeout=android_device._BOOT_SETUP_CMD_TIMEOUT_SECS
        )
        self.mock_adb.use_adb_root.assert_not_called()
        self.mock_adb.setprop.assert_called_once()
        self.mock_adb.run.assert_called_once()

    def test_enable_stay_on_while_plugged_in(self) -> None:
        """Verifies _enable_stay_on_while_plugged_in sets global stay_on_while_plugged_in to 7."""
        self.mock_adb.reset_mock()
        self.device._enable_stay_on_while_plugged_in()
        self.mock_adb.run.assert_called_once_with(
            [
                "shell",
                "settings",
                "put",
                "global",
                "stay_on_while_plugged_in",
                "7",
            ],
            timeout=android_device._BOOT_SETUP_CMD_TIMEOUT_SECS,
            attempts=1,
        )

    def test_enable_stay_on_while_plugged_in_ignores_adb_error(self) -> None:
        """Verifies _enable_stay_on_while_plugged_in logs a warning and continues on AdbError."""
        self.mock_adb.reset_mock()
        self.mock_adb.run.side_effect = adb_errors.AdbCommandError(
            "settings put failed"
        )
        with self.assertLogs(android_device._LOGGER, level="WARNING"):
            self.device._enable_stay_on_while_plugged_in()

    def test_is_sysui_running(self) -> None:
        """Verifies _is_sysui_running checks pidof <sysui_package>."""
        self.mock_adb.reset_mock()
        self.mock_adb.run.return_value = "4321\n"
        self.assertTrue(self.device._is_sysui_running())
        self.mock_adb.run.assert_called_once_with(
            ["shell", "pidof", "com.test.sysui"],
            timeout=android_device._BOOT_COMPLETED_GETPROP_TIMEOUT_SECS,
            attempts=1,
        )

        self.mock_adb.run.return_value = "   \n"
        self.assertFalse(self.device._is_sysui_running())

        self.mock_adb.run.side_effect = adb_errors.AdbCommandError(
            "pidof failed"
        )
        self.assertFalse(self.device._is_sysui_running())

    async def test_restart_sysui(self) -> None:
        """Verifies restart_sysui stops SystemUI, waits for SystemUI process, and refreshes screen size."""
        self.mock_adb.reset_mock()
        self.mock_adb.run.side_effect = ["", "4321\n"]
        with (
            mock.patch.object(
                self.device, "android_session", autospec=True
            ) as mock_session,
            mock.patch.object(
                self.device,
                "_refresh_screen_size",
                autospec=True,
            ) as mock_refresh_screen_size,
        ):
            await self.device.restart_sysui()
        mock_session.session_will_change.assert_not_called()
        mock_session.record_session.assert_not_called()
        self.mock_adb.run.assert_has_calls(
            [
                mock.call(["shell", "am", "force-stop", "com.test.sysui"]),
                mock.call(
                    ["shell", "pidof", "com.test.sysui"],
                    timeout=android_device._BOOT_COMPLETED_GETPROP_TIMEOUT_SECS,
                    attempts=1,
                ),
            ]
        )
        mock_refresh_screen_size.assert_called_once_with()

    async def test_restart_sysui_adb_error(self) -> None:
        """Verifies restart_sysui propagates AdbError when stopping SystemUI fails."""
        self.mock_adb.reset_mock()
        self.mock_adb.run.side_effect = adb_errors.AdbCommandError(
            "force-stop failed"
        )
        with self.assertRaises(adb_errors.AdbCommandError):
            await self.device.restart_sysui()

    @mock.patch.object(
        common,
        "wait_for_state",
        side_effect=errors.HoneydewTimeoutError("timed out"),
        autospec=True,
    )
    async def test_restart_sysui_timeout(
        self, mock_wait_for_state: mock.Mock
    ) -> None:
        """Verifies restart_sysui raises FuchsiaDeviceError if SystemUI fails to restart within timeout."""
        self.mock_adb.reset_mock()
        with self.assertRaises(errors.FuchsiaDeviceError):
            await self.device.restart_sysui()
        mock_wait_for_state.assert_called_once()

    async def test_restart_sysui_raises_without_sysui_package(self) -> None:
        """Verifies restart_sysui raises FuchsiaDeviceError when sysui_package is empty."""
        bare_device = android_device.AndroidDevice(
            device_info=self.dev_info,
            ffx_config_data=self.ffx_cfg,
        )
        with self.assertRaises(errors.FuchsiaDeviceError):
            await bare_device.restart_sysui()

    def test_gestures_and_buttons_affordances(self) -> None:
        """Verifies gestures and buttons affordances and cache invalidation on screen size change."""
        mock_user_input = mock.create_autospec(
            user_input_affordance.UserInput, instance=True
        )
        with mock.patch.object(
            fuchsia_device.FuchsiaDevice,
            "user_input",
            new_callable=mock.PropertyMock,
            return_value=mock_user_input,
        ):
            gestures1 = self.device.gestures
            self.assertIsInstance(gestures1, input_affordance.Gestures)
            self.assertIs(self.device.gestures, gestures1)
            self.assertEqual(gestures1._screen_size, ui_types.Size(600, 800))

            buttons = self.device.buttons
            self.assertIsInstance(buttons, input_affordance.Buttons)
            self.assertIs(self.device.buttons, buttons)

            self.mock_adb.run.return_value = "Physical size: 480x960\n"
            self.device.screen_size()

            gestures2 = self.device.gestures
            self.assertIsNot(gestures1, gestures2)
            self.assertEqual(gestures2._screen_size, ui_types.Size(480, 960))
            self.assertEqual(
                self.device._screen_size_cache, ui_types.Size(480, 960)
            )
            with (
                mock.patch.object(
                    fuchsia_device.FuchsiaDevice,
                    "on_device_boot",
                    autospec=True,
                ),
                mock.patch.object(
                    self.device,
                    "_on_boot_complete",
                    autospec=True,
                ) as mock_boot_complete,
                mock.patch.object(
                    self.device,
                    "_close_uiautomator",
                    autospec=True,
                ) as mock_close_ui,
            ):
                self.mock_adb.run.return_value = ""
                self.device._screen_size_cache = ui_types.Size(480, 960)
                self.device.__dict__["ui_navigation"] = mock.MagicMock()
                asyncio.run(self.device.on_device_boot())
                mock_boot_complete.assert_called_once_with()
                mock_close_ui.assert_called_once_with()
            self.assertIsNone(self.device._screen_size_cache)
            self.assertNotIn("gestures", self.device.__dict__)
            self.assertNotIn("buttons", self.device.__dict__)
            self.assertNotIn("ui_navigation", self.device.__dict__)

    def test_gestures_and_buttons_affordances_not_enabled(self) -> None:
        """Verifies gestures and buttons behavior when dependencies are disabled."""
        with mock.patch.object(
            fuchsia_device.FuchsiaDevice,
            "user_input",
            new_callable=mock.PropertyMock,
            side_effect=errors.NotEnabledError("user_input disabled"),
        ):
            with self.assertRaises(errors.NotEnabledError):
                _ = self.device.gestures
            buttons_no_ui = self.device.buttons
            self.assertIsInstance(buttons_no_ui, input_affordance.Buttons)
            self.assertIsNone(buttons_no_ui._user_input)
            self.device.__dict__.pop("buttons", None)

        with mock.patch.object(
            android_device.AndroidDevice,
            "adb",
            new_callable=mock.PropertyMock,
            side_effect=errors.NotEnabledError("adb disabled"),
        ):
            with self.assertRaises(errors.NotEnabledError):
                _ = self.device.buttons

        with mock.patch.object(
            fuchsia_device.FuchsiaDevice,
            "user_input",
            new_callable=mock.PropertyMock,
            side_effect=errors.NotSupportedError("user_input unsupported"),
        ):
            buttons = self.device.buttons
            self.assertIsInstance(buttons, input_affordance.Buttons)
            self.assertIsNone(buttons._user_input)

    async def test_ui_navigation_affordance_and_close_cleanup(self) -> None:
        """Verifies ui_navigation affordance and UiAutomator cleanup on close/boot."""
        nav = self.device.ui_navigation
        self.assertIsInstance(nav, ui_navigation_affordance.UiNavigation)
        self.assertIs(self.device.ui_navigation, nav)

        mock_ui_dev = mock.create_autospec(
            mobly_android_device.AndroidDevice, instance=True
        )
        mock_ui_dev.services = mock.MagicMock()
        nav._uiautomator_device = mock_ui_dev
        with mock.patch.object(
            fuchsia_device.FuchsiaDevice,
            "close",
            new_callable=mock.AsyncMock,
        ):
            await self.device.close()
        mock_ui_dev.services.stop_all.assert_called_once_with()
        self.assertIsNone(nav._uiautomator_device)

    async def test_ui_navigation_device_delegation_methods(self) -> None:
        """Verifies AndroidDevice UI navigation customization hooks."""
        mock_nav = mock.create_autospec(
            ui_navigation_affordance.UiNavigation, instance=True
        )
        mock_nav.dismiss_blocking_dialogs.return_value = True

        with mock.patch.object(
            android_device.AndroidDevice,
            "ui_navigation",
            new_callable=mock.PropertyMock,
            return_value=mock_nav,
        ):
            self.assertTrue(self.device.dismiss_blocking_dialogs())
            mock_nav.dismiss_blocking_dialogs.assert_called_once_with()


if __name__ == "__main__":
    unittest.main()
