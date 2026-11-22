# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for adb.py."""

import asyncio
import concurrent.futures
import io
import os
import signal
import socket
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
import unittest
from importlib import resources
from pathlib import Path
from typing import Any
from unittest import mock

from honeydew import errors
from honeydew.affordances.usb import usb as usb_affordance
from honeydew.transports.adb import adb, adb_server
from honeydew.transports.adb import errors as adb_errors
from honeydew.utils import host_shell
from mobly import signals

_DEVICE_NAME = "fuchsia-mock-device"
_SERIAL_NUMBER = "12345678"
_TCP_SERIAL_NUMBER = "127.0.0.1:5555"

# Arbitrary ports for `adb forward` / `adb reverse` tests.
_HOST_PORT = 15000
_DEVICE_PORT = 25000


# pylint: disable=protected-access
class AdbTests(unittest.TestCase):
    """Unit tests for ADB transport."""

    def setUp(self) -> None:
        super().setUp()

        # TODO(b/584076049): Scope verify_supported and check_connection patches
        # to self.adb_obj initialization so tests do not need to stop/restart them.
        self._verify_supported_patcher = mock.patch.object(
            adb.Adb,
            "verify_supported",
            return_value=None,
        )
        self._verify_supported_patcher.start()

        self._check_connection_patcher = mock.patch.object(
            adb.Adb,
            "check_connection",
            return_value=None,
        )
        self._check_connection_patcher.start()

        self._env_patcher = mock.patch.dict(
            os.environ,
            {
                "HONEYDEW_ADB_OVERRIDE": "/custom/adb",
                "ADB_VENDOR_KEYS": tempfile.gettempdir(),
            },
        )
        self._env_patcher.start()

        self.mock_ffx = mock.Mock()
        self.mock_usb = mock.MagicMock(spec=usb_affordance.USB)
        with mock.patch.object(adb.Adb, "_cache_adbd_pid"):
            self.adb_obj = adb.Adb(
                device_name=_DEVICE_NAME,
                serial_number=_SERIAL_NUMBER,
                ffx_transport=self.mock_ffx,
                run_isolated_server=False,
                usb=self.mock_usb,
            )

    def tearDown(self) -> None:
        self.adb_obj.close()
        self._env_patcher.stop()
        self._check_connection_patcher.stop()
        self._verify_supported_patcher.stop()
        super().tearDown()

    @mock.patch("glob.glob", autospec=True)
    def test_check_adb_sysfs_success(self, mock_glob: mock.Mock) -> None:
        """Test _check_adb_sysfs success path."""
        mock_glob.side_effect = lambda pattern: {
            "/sys/bus/usb/devices/*": ["/sys/bus/usb/devices/1-1"],
            "/sys/bus/usb/devices/1-1/*:*.*/bInterfaceClass": [
                "/sys/bus/usb/devices/1-1/1-1:1.0/bInterfaceClass"
            ],
        }.get(pattern, [])

        mock_files = {
            "/sys/bus/usb/devices/1-1/idVendor": "18d1\n",
            "/sys/bus/usb/devices/1-1/serial": f"{_SERIAL_NUMBER}\n",
            "/sys/bus/usb/devices/1-1/1-1:1.0/bInterfaceClass": "ff\n",
            "/sys/bus/usb/devices/1-1/1-1:1.0/bInterfaceSubClass": "42\n",
            "/sys/bus/usb/devices/1-1/1-1:1.0/bInterfaceProtocol": "01\n",
        }

        def mock_open_file(
            path: str, mode: str = "r", *args: Any, **kwargs: Any
        ) -> mock.MagicMock:
            if path in mock_files:
                return mock.mock_open(read_data=mock_files[path])()
            raise OSError(f"File not found: {path}")

        with mock.patch("os.path.exists", return_value=True), mock.patch(
            "builtins.open", mock_open_file
        ):
            res = adb._check_adb_sysfs(_SERIAL_NUMBER)
            self.assertTrue(res)

    @mock.patch("glob.glob", autospec=True)
    def test_check_adb_sysfs_without_serial_success(
        self, mock_glob: mock.Mock
    ) -> None:
        """Test _check_adb_sysfs success path when target_serial is None."""
        mock_glob.side_effect = lambda pattern: {
            "/sys/bus/usb/devices/*": ["/sys/bus/usb/devices/1-1"],
            "/sys/bus/usb/devices/1-1/*:*.*/bInterfaceClass": [
                "/sys/bus/usb/devices/1-1/1-1:1.0/bInterfaceClass"
            ],
        }.get(pattern, [])

        mock_files = {
            "/sys/bus/usb/devices/1-1/idVendor": "18d1\n",
            "/sys/bus/usb/devices/1-1/1-1:1.0/bInterfaceClass": "ff\n",
            "/sys/bus/usb/devices/1-1/1-1:1.0/bInterfaceSubClass": "42\n",
            "/sys/bus/usb/devices/1-1/1-1:1.0/bInterfaceProtocol": "01\n",
        }

        def mock_open_file(
            path: str, mode: str = "r", *args: Any, **kwargs: Any
        ) -> mock.MagicMock:
            if path in mock_files:
                return mock.mock_open(read_data=mock_files[path])()
            raise OSError(f"File not found: {path}")

        with mock.patch("os.path.exists", return_value=True), mock.patch(
            "builtins.open", mock_open_file
        ):
            res = adb._check_adb_sysfs(None)
            self.assertTrue(res)

    @mock.patch.object(adb, "_check_adb_sysfs", return_value=True)
    def test_verify_supported(self, mock_check: mock.Mock) -> None:
        """Test verify_supported calls _check_adb_sysfs with serial number."""
        self._verify_supported_patcher.stop()
        try:
            self.adb_obj.verify_supported()
            mock_check.assert_called_once_with(_SERIAL_NUMBER)
        finally:
            self._verify_supported_patcher.start()

    @mock.patch.object(adb, "_check_adb_sysfs", return_value=False)
    def test_verify_supported_not_supported(
        self, mock_check: mock.Mock
    ) -> None:
        """Test verify_supported raises NotSupportedError when ADB is not supported."""
        self._verify_supported_patcher.stop()
        try:
            with self.assertRaises(errors.NotSupportedError):
                self.adb_obj.verify_supported()
            mock_check.assert_called_once_with(_SERIAL_NUMBER)
        finally:
            self._verify_supported_patcher.start()

    def test_is_tcp_serial(self) -> None:
        """Test is_tcp_serial identifies TCP host:port serials."""
        self.assertTrue(adb.is_tcp_serial("127.0.0.1:5555"))
        self.assertTrue(adb.is_tcp_serial("localhost:5556"))
        self.assertTrue(adb.is_tcp_serial("[::1]:5555"))
        self.assertTrue(
            adb.is_tcp_serial("[fe80::5054:ff:fe63:5e7a%qemu]:5555")
        )
        self.assertFalse(adb.is_tcp_serial(None))
        self.assertFalse(adb.is_tcp_serial(""))
        self.assertFalse(adb.is_tcp_serial(_SERIAL_NUMBER))
        self.assertFalse(adb.is_tcp_serial(":5555"))
        self.assertFalse(adb.is_tcp_serial("127.0.0.1:0"))
        self.assertFalse(adb.is_tcp_serial("127.0.0.1:70000"))
        self.assertFalse(adb.is_tcp_serial("127.0.0.1:¹²³"))
        self.assertFalse(adb.is_tcp_serial("fe80::1"))
        self.assertFalse(adb.is_tcp_serial("[127.0.0.1]:5555"))
        self.assertFalse(adb.is_tcp_serial("[127.0.0.1:5555"))
        self.assertFalse(adb.is_tcp_serial("127.0.0.1]:5555"))
        self.assertFalse(adb.is_tcp_serial("[]:5555"))

    @mock.patch.object(
        adb, "_check_adb_sysfs", return_value=False, autospec=True
    )
    def test_verify_supported_tcp_serial_skips_sysfs(
        self, mock_check: mock.Mock
    ) -> None:
        """Test verify_supported skips sysfs check when serial is a TCP address."""
        self._verify_supported_patcher.stop()
        try:
            self.adb_obj._serial_number = _TCP_SERIAL_NUMBER
            with self.assertLogs(level="INFO") as log:
                self.adb_obj.verify_supported()
            mock_check.assert_not_called()
            self.assertTrue(
                any(
                    f"TCP ADB serial '{_TCP_SERIAL_NUMBER}' detected" in msg
                    for msg in log.output
                )
            )
        finally:
            self._verify_supported_patcher.start()

    @mock.patch(
        "socket.getaddrinfo",
        side_effect=socket.gaierror("Name or service not known"),
        autospec=True,
    )
    @mock.patch.object(
        adb, "_check_adb_sysfs", return_value=False, autospec=True
    )
    def test_verify_supported_tcp_serial_unresolvable_host_raises(
        self, mock_check: mock.Mock, mock_getaddrinfo: mock.Mock
    ) -> None:
        """Test verify_supported raises NotSupportedError on unresolvable TCP host."""
        self._verify_supported_patcher.stop()
        try:
            self.adb_obj._serial_number = "[fe80::1%qemu]:5555"
            with self.assertRaisesRegex(
                errors.NotSupportedError, "could not be resolved"
            ):
                self.adb_obj.verify_supported()
            mock_getaddrinfo.assert_called_once_with(
                "fe80::1", 5555, type=socket.SOCK_STREAM
            )
            mock_check.assert_not_called()
        finally:
            self._verify_supported_patcher.start()

    def test_public_properties(self) -> None:
        """Test serial_number, binary_path, and server_host_port properties."""
        self.assertEqual(self.adb_obj.serial_number, _SERIAL_NUMBER)
        self.assertEqual(self.adb_obj.binary_path, "/custom/adb")
        self.assertIsNone(self.adb_obj.server_host_port)

        mock_server = mock.create_autospec(adb_server.AdbServer, instance=True)
        mock_server.host.return_value = "127.0.0.1"
        mock_server.port.return_value = 54321
        self.adb_obj._adb_server = mock_server
        self.assertEqual(self.adb_obj.server_host_port, ("127.0.0.1", 54321))

    def test_restart_server(self) -> None:
        """Test restart_server restarts only the host server and reconnects.

        The USB affordance is notified around the restart, since a restart
        mid-transfer makes usb-adb-function reset the USB stack.
        """
        mock_server = mock.create_autospec(adb_server.AdbServer, instance=True)
        self.adb_obj._adb_server = mock_server
        manager = mock.Mock()
        manager.attach_mock(mock_server, "server")
        manager.attach_mock(self.mock_usb, "usb")
        with (
            mock.patch.object(
                self.adb_obj, "_restart_adbd", autospec=True
            ) as mock_restart_adbd,
            mock.patch.object(
                self.adb_obj, "check_connection"
            ) as mock_check_connection,
            mock.patch.object(
                self.adb_obj, "_cache_adbd_pid", autospec=True
            ) as mock_cache_adbd_pid,
        ):
            manager.attach_mock(mock_check_connection, "check_connection")
            manager.attach_mock(mock_cache_adbd_pid, "cache_adbd_pid")
            self.adb_obj.restart_server()
        self.assertEqual(
            manager.mock_calls,
            [
                mock.call.usb.before_disconnect(),
                mock.call.server.reset_restart_count(),
                mock.call.server.restart(),
                mock.call.check_connection(
                    timeout=adb._DEFAULT_CHECK_CONNECTION_TIMEOUT_SECS,
                    attempts=1,
                ),
                mock.call.cache_adbd_pid(),
                mock.call.usb.after_connect(),
            ],
        )
        mock_restart_adbd.assert_not_called()

    def test_restart_server_without_usb(self) -> None:
        """Test restart_server still restarts and reconnects when usb is None."""
        mock_server = mock.create_autospec(adb_server.AdbServer, instance=True)
        self.adb_obj._adb_server = mock_server
        self.adb_obj._usb = None
        with (
            mock.patch.object(
                self.adb_obj, "check_connection"
            ) as mock_check_connection,
            mock.patch.object(self.adb_obj, "_cache_adbd_pid", autospec=True),
        ):
            self.adb_obj.restart_server()
        mock_server.restart.assert_called_once_with()
        mock_check_connection.assert_called_once_with(
            timeout=adb._DEFAULT_CHECK_CONNECTION_TIMEOUT_SECS, attempts=1
        )
        self.mock_usb.before_disconnect.assert_not_called()
        self.mock_usb.after_connect.assert_not_called()

    def test_restart_server_without_isolated_server(self) -> None:
        """Test restart_server is a no-op without an isolated ADB server."""
        self.assertIsNone(self.adb_obj._adb_server)
        with mock.patch.object(
            self.adb_obj, "check_connection"
        ) as mock_check_connection:
            self.adb_obj.restart_server()
        mock_check_connection.assert_not_called()
        self.mock_usb.before_disconnect.assert_not_called()
        self.mock_usb.after_connect.assert_not_called()

    @mock.patch.object(resources, "files", autospec=True)
    def test_get_adb_binary_with_env_override(
        self, mock_files: mock.Mock
    ) -> None:
        """Test _get_adb_binary when environment variable override is provided."""
        with mock.patch.dict(
            os.environ, {"HONEYDEW_ADB_OVERRIDE": "/env/path/adb"}, clear=True
        ):
            bin_name = adb._get_adb_binary()
            self.assertEqual(bin_name, "/env/path/adb")
            mock_files.assert_not_called()

    @mock.patch.object(resources, "files", autospec=True)
    @mock.patch.object(resources, "as_file", autospec=True)
    @mock.patch("atexit.register", autospec=True)
    @mock.patch("shutil.copy2", autospec=True)
    @mock.patch("tempfile.NamedTemporaryFile", autospec=True)
    def test_get_adb_binary_with_resource_success(
        self,
        mock_tmp_file: mock.Mock,
        mock_copy: mock.Mock,
        *unused_args: Any,
    ) -> None:
        """Test _get_adb_binary when adb data resource exists."""
        with (
            mock.patch.dict(sys.modules, {"honeydew.data": mock.Mock()}),
            mock.patch.dict(os.environ, {}, clear=True),
        ):
            mock_fd = mock.Mock()
            mock_fd.name = "tmpadb"
            mock_tmp_file.return_value = mock_fd
            bin_name = adb._get_adb_binary()
            self.assertEqual(bin_name, "tmpadb")
            mock_fd.close.assert_called_once()
            mock_copy.assert_called_with(mock.ANY, bin_name)

    @mock.patch("shutil.which", return_value="/which/adb", autospec=True)
    @mock.patch.object(
        resources, "as_file", side_effect=FileNotFoundError, autospec=True
    )
    def test_get_adb_binary_fallback_to_path_success(
        self, mock_as_file: mock.Mock, mock_which: mock.Mock
    ) -> None:
        """Test _get_adb_binary falls back to PATH when resource is not available."""
        with mock.patch.dict(os.environ, {}, clear=True):
            bin_name = adb._get_adb_binary()
            self.assertEqual(bin_name, "/which/adb")
            mock_which.assert_called_once_with("adb")

    @mock.patch("shutil.which", return_value=None, autospec=True)
    @mock.patch.object(
        resources, "as_file", side_effect=FileNotFoundError, autospec=True
    )
    def test_get_adb_binary_not_found_fail(
        self, mock_as_file: mock.Mock, mock_which: mock.Mock
    ) -> None:
        """Test _get_adb_binary raises InitializationError when binary is not found anywhere."""
        with mock.patch.dict(os.environ, {}, clear=True):
            with self.assertRaises(adb_errors.InitializationError):
                adb._get_adb_binary()
            mock_which.assert_called_once_with("adb")

    @mock.patch.object(adb.Adb, "_cache_adbd_pid", autospec=True)
    @mock.patch("shutil.which", return_value="/usr/bin/adb", autospec=True)
    @mock.patch.object(
        resources, "as_file", side_effect=FileNotFoundError, autospec=True
    )
    def test_init_with_serial(
        self,
        mock_as_file: mock.Mock,
        mock_which: mock.Mock,
        mock_cache_pid: mock.Mock,
    ) -> None:
        """Test init sets adb binary and serial number."""
        with (
            mock.patch.dict(os.environ, {}, clear=True),
            mock.patch.object(adb._LOGGER, "warning") as mock_log_warning,
        ):
            obj = adb.Adb(
                device_name=_DEVICE_NAME,
                serial_number=_SERIAL_NUMBER,
                ffx_transport=self.mock_ffx,
                run_isolated_server=False,
            )
            mock_log_warning.assert_not_called()
            self.assertEqual(obj._adb_binary, "/usr/bin/adb")
            self.assertEqual(obj._serial_number, _SERIAL_NUMBER)
            mock_which.assert_called_once_with("adb")
            mock_cache_pid.assert_called_once_with(obj)

    @mock.patch("shutil.which", return_value=None, autospec=True)
    @mock.patch.object(
        resources, "as_file", side_effect=FileNotFoundError, autospec=True
    )
    def test_init_binary_not_found(
        self, mock_as_file: mock.Mock, mock_which: mock.Mock
    ) -> None:
        """Test init raises InitializationError when adb binary is not found anywhere."""
        with mock.patch.dict(os.environ, {}, clear=True):
            with self.assertRaises(adb_errors.InitializationError):
                adb.Adb(
                    device_name=_DEVICE_NAME,
                    serial_number=_SERIAL_NUMBER,
                    ffx_transport=self.mock_ffx,
                    run_isolated_server=False,
                )

    @mock.patch.object(adb, "_check_adb_sysfs", return_value=False)
    def test_init_not_supported(self, mock_check: mock.Mock) -> None:
        """Test init raises NotSupportedError when ADB is not supported."""
        self._verify_supported_patcher.stop()
        try:
            with self.assertRaises(errors.NotSupportedError):
                adb.Adb(
                    device_name=_DEVICE_NAME,
                    serial_number=_SERIAL_NUMBER,
                    ffx_transport=self.mock_ffx,
                    run_isolated_server=False,
                )
        finally:
            self._verify_supported_patcher.start()

    @mock.patch.object(adb.Adb, "_cache_adbd_pid", autospec=True)
    @mock.patch("os.path.exists", return_value=True)
    @mock.patch.object(adb, "AdbServer")
    def test_init_isolated_server(
        self,
        mock_adb_server_cls: mock.Mock,
        mock_exists: mock.Mock,
        mock_cache_pid: mock.Mock,
    ) -> None:
        """Test init starts isolated server by default and warns when output_dir is None."""
        mock_server = mock.Mock()
        mock_adb_server_cls.return_value = mock_server

        with self.assertLogs(adb._LOGGER, level="WARNING") as cm:
            obj = adb.Adb(
                device_name=_DEVICE_NAME,
                serial_number=_SERIAL_NUMBER,
                ffx_transport=self.mock_ffx,
                vendor_keys_path="/keys",
            )
        self.assertTrue(
            any("No output_dir provided" in msg for msg in cm.output)
        )
        mock_adb_server_cls.assert_called_once_with(
            adb_binary_path="/custom/adb",
            serial_id=_SERIAL_NUMBER,
            output_dir=None,
            vendor_keys_path="/keys",
        )
        mock_server.start.assert_called_once()
        mock_cache_pid.assert_called_once_with(obj)
        self.assertIsNone(obj._output_dir)
        self.assertIs(obj._adb_server, mock_server)
        obj.close()

    @mock.patch.object(adb.Adb, "_cache_adbd_pid", autospec=True)
    @mock.patch("os.path.exists", return_value=True)
    @mock.patch.object(adb, "AdbServer")
    def test_init_isolated_server_with_output_dir(
        self,
        mock_adb_server_cls: mock.Mock,
        mock_exists: mock.Mock,
        mock_cache_pid: mock.Mock,
    ) -> None:
        """Test init passes specified output_dir to AdbServer without warning."""
        mock_server = mock.Mock()
        mock_adb_server_cls.return_value = mock_server

        custom_output_dir = "/tmp/custom_output_dir"
        with mock.patch.object(adb._LOGGER, "warning") as mock_log_warning:
            obj = adb.Adb(
                device_name=_DEVICE_NAME,
                serial_number=_SERIAL_NUMBER,
                ffx_transport=self.mock_ffx,
                output_dir=custom_output_dir,
                vendor_keys_path="/keys",
            )
            mock_log_warning.assert_not_called()
        mock_adb_server_cls.assert_called_once_with(
            adb_binary_path="/custom/adb",
            serial_id=_SERIAL_NUMBER,
            output_dir=Path(custom_output_dir),
            vendor_keys_path="/keys",
        )
        self.assertEqual(obj._output_dir, Path(custom_output_dir))
        mock_server.start.assert_called_once()
        mock_cache_pid.assert_called_once_with(obj)
        self.assertIs(obj._adb_server, mock_server)
        obj.close()

    @mock.patch.object(adb.Adb, "_cache_adbd_pid", autospec=True)
    @mock.patch("os.path.exists", return_value=True)
    @mock.patch.object(adb, "AdbServer")
    def test_init_isolated_server_with_empty_output_dir(
        self,
        mock_adb_server_cls: mock.Mock,
        mock_exists: mock.Mock,
        mock_cache_pid: mock.Mock,
    ) -> None:
        """Test init treats empty output_dir as unspecified and logs a warning."""
        mock_server = mock.Mock()
        mock_adb_server_cls.return_value = mock_server

        with self.assertLogs(adb._LOGGER, level="WARNING") as cm:
            obj = adb.Adb(
                device_name=_DEVICE_NAME,
                serial_number=_SERIAL_NUMBER,
                ffx_transport=self.mock_ffx,
                output_dir="",
                vendor_keys_path="/keys",
            )
        self.assertTrue(
            any("No output_dir provided" in msg for msg in cm.output)
        )
        mock_adb_server_cls.assert_called_once_with(
            adb_binary_path="/custom/adb",
            serial_id=_SERIAL_NUMBER,
            output_dir=None,
            vendor_keys_path="/keys",
        )
        self.assertIsNone(obj._output_dir)
        obj.close()

    @mock.patch.object(adb, "AdbServer")
    def test_init_isolated_server_error(
        self, mock_adb_server_cls: mock.Mock
    ) -> None:
        """Test init raises InitializationError when isolated server fails to start."""
        mock_server = mock.Mock()
        mock_server.start.side_effect = adb_errors.AdbServerError(
            "Failed to start server"
        )
        mock_adb_server_cls.return_value = mock_server

        with self.assertRaises(adb_errors.InitializationError):
            adb.Adb(
                device_name=_DEVICE_NAME,
                serial_number=_SERIAL_NUMBER,
                ffx_transport=self.mock_ffx,
            )

    def test_init_calls_check_connection(self) -> None:
        """Test init calls check_connection and _cache_adbd_pid."""
        mock_check = mock.Mock()
        mock_cache = mock.Mock()
        with (
            mock.patch.object(adb.Adb, "check_connection", mock_check),
            mock.patch.object(adb.Adb, "_cache_adbd_pid", mock_cache),
        ):
            adb.Adb(
                device_name=_DEVICE_NAME,
                serial_number=_SERIAL_NUMBER,
                ffx_transport=self.mock_ffx,
                run_isolated_server=False,
            )
        mock_check.assert_called_once_with(
            timeout=adb._DEFAULT_CHECK_CONNECTION_TIMEOUT_SECS
        )
        mock_cache.assert_called_once()

    def test_on_device_boot(self) -> None:
        """Test on_device_boot resets root state, checks connection, and caches adbd PID."""
        self.adb_obj._is_root = True
        self.adb_obj._rooted_by_context = True
        self.adb_obj._root_ref_count = 2
        self.adb_obj._cached_adbd_pid = "1111"

        with (
            mock.patch.object(adb.Adb, "check_connection") as mock_check,
            mock.patch.object(adb.Adb, "_cache_adbd_pid") as mock_cache,
        ):
            self.adb_obj.on_device_boot()
            mock_check.assert_called_once_with(
                timeout=adb._DEFAULT_CHECK_CONNECTION_TIMEOUT_SECS
            )
            mock_cache.assert_called_once_with()

        self.assertFalse(self.adb_obj._is_root)
        self.assertFalse(self.adb_obj._rooted_by_context)
        self.assertEqual(self.adb_obj._root_ref_count, 0)

    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_check_connection_success(
        self,
        mock_run: mock.Mock,
    ) -> None:
        """Test check_connection runs wait-for-device with the specified timeout."""
        self._check_connection_patcher.stop()
        try:
            self.adb_obj.check_connection(timeout=10.0)
            mock_run.assert_called_once_with(
                self.adb_obj,
                ["wait-for-device"],
                timeout=10.0,
                attempts=adb._DEFAULT_RUN_ATTEMPTS,
                log_devices_on_failure=False,
            )
        finally:
            self._check_connection_patcher.start()

    @mock.patch.object(
        host_shell,
        "run",
        return_value=f"List of devices attached\n{_TCP_SERIAL_NUMBER}\toffline\n",
        autospec=True,
    )
    @mock.patch("time.time", side_effect=[100.0, 110.0], autospec=True)
    @mock.patch.object(adb.Adb, "run", autospec=True)
    @mock.patch.object(adb.Adb, "_connect_tcp", autospec=True)
    def test_check_connection_tcp_timeout_expired(
        self,
        mock_connect_tcp: mock.Mock,
        mock_run: mock.Mock,
        mock_time: mock.Mock,
        mock_host_shell_run: mock.Mock,
    ) -> None:
        """Test check_connection logs adb devices -l and raises AdbConnectionError if timeout expires during connect_tcp."""
        self.adb_obj._serial_number = _TCP_SERIAL_NUMBER
        self._check_connection_patcher.stop()
        try:
            with (
                self.assertLogs(adb._LOGGER, level="INFO") as logs,
                self.assertRaises(adb_errors.AdbConnectionError) as context,
            ):
                self.adb_obj.check_connection(timeout=10.0)
            self.assertIn("connect_tcp", str(context.exception))
            self.assertNotIn("List of devices attached", str(context.exception))
            self.assertTrue(
                any(
                    f"(serial {_TCP_SERIAL_NUMBER}) state on host: offline"
                    in msg
                    for msg in logs.output
                )
            )
            self.assertIsInstance(
                context.exception.__cause__, adb_errors.AdbTimeoutError
            )
            mock_run.assert_not_called()
            mock_host_shell_run.assert_called_once_with(
                cmd=["/custom/adb", "devices", "-l"],
                capture_output=True,
                capture_error_in_output=True,
                timeout=adb._COLLECT_ADB_DEVICES_TIMEOUT_SECS,
                env=mock.ANY,
            )
        finally:
            self._check_connection_patcher.start()

    @mock.patch.object(
        host_shell,
        "run",
        return_value="List of devices attached\n\n",
        autospec=True,
    )
    @mock.patch.object(adb.Adb, "run", autospec=True)
    @mock.patch.object(adb.Adb, "_connect_tcp", autospec=True)
    def test_check_connection_tcp_connect_failure_logs_adb_devices(
        self,
        mock_connect_tcp: mock.Mock,
        mock_run: mock.Mock,
        mock_host_shell_run: mock.Mock,
    ) -> None:
        """Test check_connection logs adb devices -l and re-raises the original error when _connect_tcp raises."""
        self.adb_obj._serial_number = _TCP_SERIAL_NUMBER
        self._check_connection_patcher.stop()
        try:
            for err_cls, err_msg in (
                (adb_errors.AdbConnectionError, "tcp connect timed out"),
                (adb_errors.AdbUnauthorizedError, "tcp unauthorized"),
            ):
                with self.subTest(err_cls=err_cls):
                    mock_host_shell_run.reset_mock()
                    cause = errors.HostCmdError("underlying connect error")
                    connect_err = err_cls(err_msg)
                    connect_err.__cause__ = cause
                    mock_connect_tcp.side_effect = connect_err
                    with (
                        self.assertLogs(adb._LOGGER, level="INFO") as logs,
                        self.assertRaises(err_cls) as ctx,
                    ):
                        self.adb_obj.check_connection(timeout=10.0)
                    self.assertIs(ctx.exception, connect_err)
                    self.assertEqual(str(ctx.exception), err_msg)
                    self.assertIs(ctx.exception.__cause__, cause)
                    self.assertTrue(
                        any(
                            f"(serial {_TCP_SERIAL_NUMBER}) state on host: "
                            "not listed" in msg
                            for msg in logs.output
                        )
                    )
                    mock_run.assert_not_called()
                    mock_host_shell_run.assert_called_once()
        finally:
            self._check_connection_patcher.start()

    @mock.patch.object(host_shell, "run", autospec=True)
    def test_check_connection_wait_for_device_failure_collects_once(
        self,
        mock_host_shell_run: mock.Mock,
    ) -> None:
        """Test check_connection logs adb devices -l once when wait-for-device fails."""
        self._check_connection_patcher.stop()
        try:
            mock_server = mock.create_autospec(
                adb_server.AdbServer, instance=True
            )
            mock_server.host.return_value = "127.0.0.1"
            mock_server.port.return_value = 5037
            # 3 failed wait-for-device attempts (adb._DEFAULT_RUN_ATTEMPTS = 3),
            # followed by 1 `adb devices -l` call after the final attempt.
            mock_host_shell_run.side_effect = [
                errors.HoneydewTimeoutError("timed out 1"),
                errors.HoneydewTimeoutError("timed out 2"),
                errors.HoneydewTimeoutError("timed out 3"),
                f"List of devices attached\n{_SERIAL_NUMBER}\toffline\n",
            ]
            with (
                mock.patch.object(self.adb_obj, "_adb_server", mock_server),
                mock.patch.object(
                    self.adb_obj, "_recover_adb_server", autospec=True
                ) as mock_recover,
                self.assertLogs(adb._LOGGER, level="INFO") as logs,
                self.assertRaises(adb_errors.AdbConnectionError) as context,
            ):
                self.adb_obj.check_connection(timeout=10.0)
            self.assertEqual(mock_recover.call_count, 2)
            self.assertNotIn("List of devices attached", str(context.exception))
            self.assertIn(
                "(adb state: offline) with err:", str(context.exception)
            )
            self.assertEqual(
                sum("ADB Triage:" in msg for msg in logs.output), 1
            )
            self.assertTrue(
                any(
                    f"(serial {_SERIAL_NUMBER}) state on host: offline" in msg
                    for msg in logs.output
                )
            )
            base_cmd = ["/custom/adb", "-H", "127.0.0.1", "-P", "5037"]
            wait_cmd = [*base_cmd, "-s", _SERIAL_NUMBER, "wait-for-device"]
            devices_cmd = [*base_cmd, "devices", "-l"]
            self.assertEqual(
                [
                    call.kwargs["cmd"]
                    for call in mock_host_shell_run.call_args_list
                ],
                [wait_cmd, wait_cmd, wait_cmd, devices_cmd],
            )
        finally:
            self._check_connection_patcher.start()

    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_check_connection_failure_wait_for_device(
        self, mock_run: mock.Mock
    ) -> None:
        """Test check_connection raises AdbConnectionError with the adb state when wait-for-device fails."""
        self._check_connection_patcher.stop()
        try:
            for adb_state, expected_msg in (
                (
                    "offline",
                    f"ADB connection check failed for {self.adb_obj._device_name} "
                    "(adb state: offline) with err: wait-for-device failed",
                ),
                # No state is added if `adb devices -l` itself fails.
                (
                    None,
                    f"ADB connection check failed for {self.adb_obj._device_name} "
                    "with err: wait-for-device failed",
                ),
            ):
                with self.subTest(adb_state=adb_state):
                    mock_run.side_effect = adb_errors.AdbCommandError(
                        "wait-for-device failed"
                    )
                    with (
                        mock.patch.object(
                            self.adb_obj,
                            "_log_adb_devices",
                            return_value=adb_state,
                            autospec=True,
                        ) as mock_log_adb_devices,
                        self.assertRaises(
                            adb_errors.AdbConnectionError
                        ) as context,
                    ):
                        self.adb_obj.check_connection(timeout=10.0)
                    self.assertEqual(str(context.exception), expected_msg)
                    self.assertIs(
                        context.exception.__cause__, mock_run.side_effect
                    )
                    mock_log_adb_devices.assert_called_once()
        finally:
            self._check_connection_patcher.start()

    @mock.patch("time.time", side_effect=[100.0, 102.0], autospec=True)
    @mock.patch.object(adb.Adb, "run", autospec=True)
    @mock.patch.object(adb.Adb, "_connect_tcp", autospec=True)
    def test_check_connection_tcp_serial(
        self,
        mock_connect_tcp: mock.Mock,
        mock_run: mock.Mock,
        mock_time: mock.Mock,
    ) -> None:
        """Test check_connection connects over TCP before wait-for-device."""
        self.adb_obj._serial_number = _TCP_SERIAL_NUMBER
        self._check_connection_patcher.stop()
        try:
            self.adb_obj.check_connection(timeout=10.0)
            mock_connect_tcp.assert_called_once_with(self.adb_obj, timeout=10.0)
            mock_run.assert_called_once_with(
                self.adb_obj,
                ["wait-for-device"],
                timeout=8.0,
                attempts=adb._DEFAULT_RUN_ATTEMPTS,
                log_devices_on_failure=False,
            )
        finally:
            self._check_connection_patcher.start()

    @mock.patch.object(host_shell, "run", autospec=True)
    def test_connect_tcp_success(self, mock_host_shell_run: mock.Mock) -> None:
        """Test _connect_tcp succeeds when adb connect reports connected."""
        self.adb_obj._serial_number = _TCP_SERIAL_NUMBER
        mock_host_shell_run.return_value = (
            f"connected to {_TCP_SERIAL_NUMBER}\n"
        )

        self.adb_obj._connect_tcp(timeout=5.0)

        mock_host_shell_run.assert_called_once_with(
            cmd=["/custom/adb", "connect", _TCP_SERIAL_NUMBER],
            capture_output=True,
            capture_error_in_output=True,
            timeout=5.0,
            env=mock.ANY,
        )

    @mock.patch("time.sleep", autospec=True)
    @mock.patch.object(host_shell, "run", autospec=True)
    def test_connect_tcp_polls_until_connected(
        self, mock_host_shell_run: mock.Mock, mock_sleep: mock.Mock
    ) -> None:
        """Test _connect_tcp polls when initial connect attempt fails."""
        self.adb_obj._serial_number = _TCP_SERIAL_NUMBER
        mock_host_shell_run.side_effect = [
            f"failed to connect to '{_TCP_SERIAL_NUMBER}': Connection refused\n",
            f"unable to connect to {_TCP_SERIAL_NUMBER}: No route to host\n",
            errors.HostCmdError("adb connect failed"),
            f"already connected to {_TCP_SERIAL_NUMBER}\n",
        ]

        self.adb_obj._connect_tcp(timeout=5.0, poll_interval=0.5)

        self.assertEqual(mock_host_shell_run.call_count, 4)
        mock_sleep.assert_has_calls(
            [mock.call(0.5), mock.call(0.5), mock.call(0.5)]
        )

    @mock.patch("time.sleep", autospec=True)
    @mock.patch("time.time", side_effect=[0.0, 1.0, 10.0], autospec=True)
    @mock.patch.object(host_shell, "run", autospec=True)
    def test_connect_tcp_timeout(
        self,
        mock_host_shell_run: mock.Mock,
        mock_time: mock.Mock,
        mock_sleep: mock.Mock,
    ) -> None:
        """Test _connect_tcp raises AdbConnectionError with last error on timeout."""
        self.adb_obj._serial_number = _TCP_SERIAL_NUMBER
        mock_host_shell_run.return_value = (
            f"failed to connect to '{_TCP_SERIAL_NUMBER}': Connection refused\n"
        )

        with self.assertRaisesRegex(
            adb_errors.AdbConnectionError, "Connection refused"
        ):
            self.adb_obj._connect_tcp(timeout=5.0)

    @mock.patch.object(host_shell, "run", autospec=True)
    def test_connect_tcp_non_positive_timeout_raises(
        self, mock_host_shell_run: mock.Mock
    ) -> None:
        """Test _connect_tcp raises AdbConnectionError immediately when timeout <= 0."""
        self.adb_obj._serial_number = _TCP_SERIAL_NUMBER
        for bad_timeout in (0.0, -1.0):
            with self.subTest(timeout=bad_timeout):
                with self.assertRaises(adb_errors.AdbConnectionError):
                    self.adb_obj._connect_tcp(timeout=bad_timeout)
        mock_host_shell_run.assert_not_called()

    @mock.patch.object(host_shell, "run", autospec=True)
    def test_connect_tcp_unauthorized_fails_fast(
        self, mock_host_shell_run: mock.Mock
    ) -> None:
        """Test _connect_tcp raises AdbUnauthorizedError immediately when unauthorized or auth fails."""
        self.adb_obj._serial_number = _TCP_SERIAL_NUMBER
        for bad_output in (
            "error: device unauthorized.\n",
            f"failed to authenticate to {_TCP_SERIAL_NUMBER}\n",
        ):
            with self.subTest(output=bad_output):
                mock_host_shell_run.reset_mock()
                mock_host_shell_run.return_value = bad_output
                with self.assertRaises(adb_errors.AdbUnauthorizedError):
                    self.adb_obj._connect_tcp(timeout=5.0)
                mock_host_shell_run.assert_called_once()

        for bad_err in (
            "error: device unauthorized.",
            f"failed to authenticate to {_TCP_SERIAL_NUMBER}",
        ):
            with self.subTest(err=bad_err):
                mock_host_shell_run.reset_mock()
                mock_host_shell_run.side_effect = errors.HostCmdError(bad_err)
                with self.assertRaises(adb_errors.AdbUnauthorizedError):
                    self.adb_obj._connect_tcp(timeout=5.0)
                mock_host_shell_run.assert_called_once()

    @mock.patch.object(host_shell, "run", autospec=True)
    def test_run_success(self, mock_host_shell_run: mock.Mock) -> None:
        """Test run success path."""
        mock_host_shell_run.return_value = "output_text"

        output = self.adb_obj.run(["shell", "echo", "hello"])

        expected_env = os.environ.copy()
        expected_env["ADB_VENDOR_KEYS"] = tempfile.gettempdir()
        self.assertEqual(output, "output_text")
        mock_host_shell_run.assert_called_once_with(
            cmd=["/custom/adb", "-s", _SERIAL_NUMBER, "shell", "echo", "hello"],
            capture_output=True,
            capture_error_in_output=True,
            timeout=None,
            env=expected_env,
        )

    @mock.patch.object(host_shell, "run", autospec=True)
    def test_run_without_serial(self, mock_host_shell_run: mock.Mock) -> None:
        """Test run with include_serial=False."""
        mock_host_shell_run.return_value = "output_text"

        output = self.adb_obj.run(["devices"], include_serial=False)

        expected_env = os.environ.copy()
        expected_env["ADB_VENDOR_KEYS"] = tempfile.gettempdir()
        self.assertEqual(output, "output_text")
        mock_host_shell_run.assert_called_once_with(
            cmd=["/custom/adb", "devices"],
            capture_output=True,
            capture_error_in_output=True,
            timeout=None,
            env=expected_env,
        )

    @mock.patch.object(
        host_shell,
        "run",
        side_effect=errors.HostCmdError("error_text"),
        autospec=True,
    )
    def test_run_failure(self, mock_host_shell_run: mock.Mock) -> None:
        """Test run failure path (non-zero exit code)."""
        with self.assertRaises(adb_errors.AdbCommandError) as context:
            self.adb_obj.run(["shell", "bad_command"])

        self.assertIn("error_text", str(context.exception))

    @mock.patch.object(host_shell, "run", autospec=True)
    def test_log_adb_devices_success(
        self, mock_host_shell_run: mock.Mock
    ) -> None:
        """Test _log_adb_devices runs adb devices -l, logs output and device state, and returns the state."""
        mock_host_shell_run.return_value = (
            f"List of devices attached\n{_SERIAL_NUMBER}\tdevice\n"
        )
        with self.assertLogs(adb._LOGGER, level="INFO") as cm:
            state = self.adb_obj._log_adb_devices()
        self.assertEqual(state, "device")
        self.assertTrue(
            any(
                f"Host `adb devices -l` output (checking for "
                f"{self.adb_obj._device_name}):\nList of devices attached\n"
                f"{_SERIAL_NUMBER}\tdevice" in msg
                for msg in cm.output
            )
        )
        self.assertTrue(
            any(
                f"(serial {_SERIAL_NUMBER}) state on host: device" in msg
                for msg in cm.output
            )
        )
        mock_host_shell_run.assert_called_once_with(
            cmd=["/custom/adb", "devices", "-l"],
            capture_output=True,
            capture_error_in_output=True,
            timeout=adb._COLLECT_ADB_DEVICES_TIMEOUT_SECS,
            env=mock.ANY,
        )

    def test_get_device_state_from_adb_devices(self) -> None:
        """Test _get_device_state_from_adb_devices parses this device's state."""
        header = "List of devices attached"
        cases: list[tuple[str, str]] = [
            (
                f"{header}\n{_SERIAL_NUMBER}\tdevice usb:1-1 product:p model:m",
                "device",
            ),
            (f"{header}\n{_SERIAL_NUMBER}\toffline", "offline"),
            (f"{header}\n{_SERIAL_NUMBER}\tunauthorized", "unauthorized"),
            (
                f"{header}\n{_SERIAL_NUMBER}\tno permissions (user in plugdev "
                "group; are your udev rules wrong?); see [http://dev/udev]",
                "no permissions",
            ),
            (f"{header}\nother_serial\tdevice", "not listed"),
            # A serial that merely starts with ours must not match.
            (f"{header}\n{_SERIAL_NUMBER}9\tdevice", "not listed"),
            (header, "not listed"),
            ("", "not listed"),
        ]
        for devices_output, expected_state in cases:
            with self.subTest(devices_output=devices_output):
                self.assertEqual(
                    self.adb_obj._get_device_state_from_adb_devices(
                        devices_output
                    ),
                    expected_state,
                )

    @mock.patch.object(
        host_shell,
        "run",
        side_effect=errors.HostCmdError("devices failed"),
        autospec=True,
    )
    def test_log_adb_devices_failure(
        self, mock_host_shell_run: mock.Mock
    ) -> None:
        """Test _log_adb_devices logs a warning, returns None and does not raise when HoneydewError is raised."""
        with self.assertLogs(adb._LOGGER, level="WARNING") as cm:
            self.assertIsNone(self.adb_obj._log_adb_devices())
        self.assertTrue(
            any(
                "Failed to collect `adb devices -l`" in msg for msg in cm.output
            )
        )
        mock_host_shell_run.assert_called_once()

    @mock.patch.object(host_shell, "run", autospec=True)
    def test_wait_for_device_failure_logs_adb_devices_output(
        self, mock_host_shell_run: mock.Mock
    ) -> None:
        """Test wait-for-device single-attempt failure logs adb devices -l and raises the original error."""
        cases: list[
            tuple[
                Exception,
                str,
                type[adb_errors.AdbError],
                str,
                float | None,
            ]
        ] = [
            (
                errors.HoneydewTimeoutError("timed out"),
                f"List of devices attached\n{_SERIAL_NUMBER}\toffline\n",
                adb_errors.AdbTimeoutError,
                "offline",
                10.0,
            ),
            (
                errors.HostCmdError("error: device unauthorized."),
                f"List of devices attached\n{_SERIAL_NUMBER}\tunauthorized\n",
                adb_errors.AdbUnauthorizedError,
                "unauthorized",
                None,
            ),
            (
                errors.HostCmdError("error: device not found"),
                "List of devices attached\n\n",
                adb_errors.AdbCommandError,
                "not listed",
                None,
            ),
            (
                errors.HoneydewError("unexpected transport failure"),
                f"List of devices attached\n{_SERIAL_NUMBER}\tno permissions\n",
                adb_errors.AdbCommandError,
                "no permissions",
                None,
            ),
        ]
        for (
            underlying_err,
            devices_output,
            expected_err_cls,
            expected_state,
            timeout,
        ) in cases:
            with self.subTest(underlying_err=underlying_err):
                mock_host_shell_run.reset_mock()
                mock_host_shell_run.side_effect = [
                    underlying_err,
                    devices_output,
                ]
                with (
                    self.assertLogs(adb._LOGGER, level="INFO") as logs,
                    self.assertRaises(expected_err_cls) as ctx,
                ):
                    self.adb_obj.run(
                        ["wait-for-device"],
                        timeout=timeout,
                        attempts=1,
                        log_devices_on_failure=True,
                    )
                self.assertNotIn("List of devices attached", str(ctx.exception))
                self.assertIs(ctx.exception.__cause__, underlying_err)
                self.assertEqual(mock_host_shell_run.call_count, 2)
                self.assertTrue(
                    any(
                        f"(serial {_SERIAL_NUMBER}) state on host: "
                        f"{expected_state}" in msg
                        for msg in logs.output
                    )
                )

    @mock.patch.object(host_shell, "run", autospec=True)
    def test_wait_for_device_failure_when_adb_devices_collection_fails(
        self, mock_host_shell_run: mock.Mock
    ) -> None:
        """Test wait-for-device failure still raises original error if adb devices -l fails."""
        underlying_err = errors.HoneydewTimeoutError("timed out")
        mock_host_shell_run.side_effect = [
            underlying_err,
            errors.HostCmdError("adb devices failed"),
        ]
        with self.assertRaises(adb_errors.AdbTimeoutError) as ctx:
            self.adb_obj.run(
                ["wait-for-device"],
                timeout=10.0,
                attempts=1,
                log_devices_on_failure=True,
            )
        self.assertIn("timed out", str(ctx.exception))
        self.assertIs(ctx.exception.__cause__, underlying_err)
        self.assertEqual(mock_host_shell_run.call_count, 2)

    @mock.patch.object(host_shell, "run", autospec=True)
    def test_run_failure_does_not_log_adb_devices_by_default(
        self, mock_host_shell_run: mock.Mock
    ) -> None:
        """Test run does not run adb devices -l on failure unless log_devices_on_failure is set."""
        for cmd in (["wait-for-device"], ["shell", "id"]):
            with self.subTest(cmd=cmd):
                mock_host_shell_run.reset_mock()
                underlying_err = errors.HostCmdError("error: device not found")
                mock_host_shell_run.side_effect = [underlying_err]
                with (
                    mock.patch.object(
                        self.adb_obj, "_log_adb_devices", autospec=True
                    ) as mock_log_adb_devices,
                    self.assertRaises(adb_errors.AdbCommandError) as ctx,
                ):
                    self.adb_obj.run(cmd, attempts=1)
                self.assertIs(ctx.exception.__cause__, underlying_err)
                mock_host_shell_run.assert_called_once()
                mock_log_adb_devices.assert_not_called()

    @mock.patch.object(host_shell, "run", autospec=True)
    def test_wait_for_device_multi_attempt_failure_collects_adb_devices_once(
        self, mock_host_shell_run: mock.Mock
    ) -> None:
        """Test multi-attempt wait-for-device failure logs adb devices -l only once after final attempt."""
        mock_server = mock.create_autospec(adb_server.AdbServer, instance=True)
        mock_server.host.return_value = "127.0.0.1"
        mock_server.port.return_value = 5037
        # 3 failed wait-for-device attempts (adb._DEFAULT_RUN_ATTEMPTS = 3),
        # followed by 1 `adb devices -l` call after the final attempt.
        mock_host_shell_run.side_effect = [
            errors.HostCmdError("error: device not found"),
            errors.HostCmdError("error: device not found"),
            errors.HostCmdError("error: device not found"),
            "List of devices attached\n\n",
        ]
        with (
            mock.patch.object(self.adb_obj, "_adb_server", mock_server),
            mock.patch.object(
                self.adb_obj, "_recover_adb_server", autospec=True
            ) as mock_recover,
            self.assertRaises(adb_errors.AdbCommandError) as multi_ctx,
        ):
            self.adb_obj.run(["wait-for-device"], log_devices_on_failure=True)
        self.assertEqual(mock_recover.call_count, 2)
        self.assertEqual(
            str(multi_ctx.exception),
            "error: device not found",
        )
        base_cmd = ["/custom/adb", "-H", "127.0.0.1", "-P", "5037"]
        wait_cmd = [*base_cmd, "-s", _SERIAL_NUMBER, "wait-for-device"]
        devices_cmd = [*base_cmd, "devices", "-l"]
        self.assertEqual(
            [call.kwargs["cmd"] for call in mock_host_shell_run.call_args_list],
            [wait_cmd, wait_cmd, wait_cmd, devices_cmd],
        )

    @mock.patch.object(host_shell, "run", autospec=True)
    def test_wait_for_device_server_recovery_error_logs_adb_devices_output(
        self, mock_host_shell_run: mock.Mock
    ) -> None:
        """Test wait-for-device logs adb devices -l when _recover_adb_server raises."""
        mock_server = mock.create_autospec(adb_server.AdbServer, instance=True)
        mock_server.host.return_value = "127.0.0.1"
        mock_server.port.return_value = 5037
        mock_host_shell_run.side_effect = [
            errors.HoneydewTimeoutError("timed out"),
            f"List of devices attached\n{_SERIAL_NUMBER}\toffline\n",
        ]
        with (
            mock.patch.object(self.adb_obj, "_adb_server", mock_server),
            mock.patch.object(
                self.adb_obj,
                "_recover_adb_server",
                side_effect=adb_errors.AdbConnectionError(
                    "TCP reconnect failed after server restart"
                ),
                autospec=True,
            ),
            self.assertLogs(adb._LOGGER, level="INFO") as logs,
            self.assertRaises(
                adb_errors.AdbConnectionError
            ) as recover_conn_ctx,
        ):
            self.adb_obj.run(["wait-for-device"], log_devices_on_failure=True)
        self.assertEqual(
            str(recover_conn_ctx.exception),
            "TCP reconnect failed after server restart",
        )
        self.assertTrue(
            any(
                f"(serial {_SERIAL_NUMBER}) state on host: offline" in msg
                for msg in logs.output
            )
        )

    @mock.patch.object(host_shell, "run", autospec=True)
    def test_wait_for_device_tcp_reconnect_unauthorized_logs_adb_devices_output(
        self, mock_host_shell_run: mock.Mock
    ) -> None:
        """Test wait-for-device logs adb devices -l when TCP retry _connect_tcp raises AdbUnauthorizedError."""
        mock_host_shell_run.side_effect = [
            errors.HostCmdError("error: connection reset"),
            f"List of devices attached\n{_TCP_SERIAL_NUMBER}\tunauthorized\n",
        ]
        with (
            mock.patch.object(
                self.adb_obj, "_serial_number", _TCP_SERIAL_NUMBER
            ),
            mock.patch.object(
                self.adb_obj,
                "_connect_tcp",
                side_effect=adb_errors.AdbUnauthorizedError(
                    "TCP target unauthorized"
                ),
                autospec=True,
            ),
            self.assertLogs(adb._LOGGER, level="INFO") as logs,
            self.assertRaises(
                adb_errors.AdbUnauthorizedError
            ) as tcp_unauth_ctx,
        ):
            self.adb_obj.run(["wait-for-device"], log_devices_on_failure=True)
        self.assertEqual(
            str(tcp_unauth_ctx.exception),
            "TCP target unauthorized",
        )
        self.assertTrue(
            any(
                f"(serial {_TCP_SERIAL_NUMBER}) state on host: unauthorized"
                in msg
                for msg in logs.output
            )
        )

    @mock.patch.object(host_shell, "run", autospec=True)
    def test_wait_for_device_offline_reconnect_unauthorized_logs_adb_devices_output(
        self, mock_host_shell_run: mock.Mock
    ) -> None:
        """Test wait-for-device logs adb devices -l when _reconnect_offline raises AdbUnauthorizedError."""
        mock_host_shell_run.side_effect = [
            errors.HostCmdError("error: device offline"),
            f"List of devices attached\n{_SERIAL_NUMBER}\tunauthorized\n",
        ]
        with (
            mock.patch.object(
                self.adb_obj,
                "_reconnect_offline",
                side_effect=adb_errors.AdbUnauthorizedError(
                    "Offline reconnect unauthorized"
                ),
                autospec=True,
            ),
            self.assertLogs(adb._LOGGER, level="INFO") as logs,
            self.assertRaises(
                adb_errors.AdbUnauthorizedError
            ) as offline_unauth_ctx,
        ):
            self.adb_obj.run(["wait-for-device"], log_devices_on_failure=True)
        self.assertEqual(
            str(offline_unauth_ctx.exception),
            "Offline reconnect unauthorized",
        )
        self.assertTrue(
            any(
                f"(serial {_SERIAL_NUMBER}) state on host: unauthorized" in msg
                for msg in logs.output
            )
        )

    @mock.patch.object(
        host_shell,
        "run",
        side_effect=signals.TestAbortAll("abort"),
        autospec=True,
    )
    def test_run_does_not_wrap_unexpected_exceptions(
        self, mock_host_shell_run: mock.Mock
    ) -> None:
        """Test run propagates non-host-command exceptions (e.g. Mobly signals) as-is."""
        with self.assertRaises(signals.TestAbortAll):
            self.adb_obj.run(["shell", "id"])

    @mock.patch.object(
        host_shell,
        "run",
        side_effect=signals.TestAbortAll("abort"),
        autospec=True,
    )
    def test_cache_adbd_pid_does_not_swallow_unexpected_exceptions(
        self, mock_host_shell_run: mock.Mock
    ) -> None:
        """Test _cache_adbd_pid propagates non-host-command exceptions."""
        with self.assertRaises(signals.TestAbortAll):
            self.adb_obj._cache_adbd_pid()

    @mock.patch.object(
        adb.Adb, "run", side_effect=signals.TestAbortAll("abort"), autospec=True
    )
    def test_check_connection_does_not_wrap_unexpected_exceptions(
        self, mock_run: mock.Mock
    ) -> None:
        """Test check_connection propagates non-host-command exceptions as-is."""
        self._check_connection_patcher.stop()
        try:
            with self.assertRaises(signals.TestAbortAll):
                self.adb_obj.check_connection(timeout=10.0)
        finally:
            self._check_connection_patcher.start()

    @mock.patch.object(
        host_shell,
        "run",
        side_effect=errors.HostCmdError("error: device unauthorized."),
        autospec=True,
    )
    def test_run_unauthorized_error(
        self, mock_host_shell_run: mock.Mock
    ) -> None:
        """Test run raises AdbUnauthorizedError immediately without restarting server."""
        mock_server = mock.Mock()
        mock_server.port.return_value = 12345
        self.adb_obj._adb_server = mock_server

        with self.assertRaises(adb_errors.AdbUnauthorizedError) as context:
            self.adb_obj.run(["shell", "id"])

        self.assertIn("is unauthorized", str(context.exception))
        self.assertIn("ADB_VENDOR_KEYS", str(context.exception))
        mock_host_shell_run.assert_called_once()
        mock_server.restart.assert_not_called()

    @mock.patch.object(
        adb.Adb,
        "run",
        side_effect=adb_errors.AdbUnauthorizedError("Device unauthorized"),
        autospec=True,
    )
    def test_check_connection_unauthorized_error(
        self, mock_run: mock.Mock
    ) -> None:
        """Test check_connection propagates AdbUnauthorizedError."""
        self._check_connection_patcher.stop()
        try:
            with self.assertRaises(adb_errors.AdbUnauthorizedError):
                self.adb_obj.check_connection(timeout=10.0)
        finally:
            self._check_connection_patcher.start()

    @mock.patch.object(adb.Adb, "_recover_adb_server", autospec=True)
    @mock.patch.object(
        host_shell,
        "run",
        side_effect=errors.HoneydewTimeoutError("timed out"),
        autospec=True,
    )
    def test_run_timeout(
        self,
        mock_host_shell_run: mock.Mock,
        mock_recover_adb_server: mock.Mock,
    ) -> None:
        """Test run timeout path passes timeout to each attempt."""
        mock_server = mock.Mock()
        mock_server.host.return_value = "127.0.0.1"
        mock_server.port.return_value = 12345
        self.adb_obj._adb_server = mock_server

        with self.assertRaises(adb_errors.AdbTimeoutError):
            self.adb_obj.run(["shell", "long_running"], timeout=30.0)

        expected_call = mock.call(
            cmd=[
                "/custom/adb",
                "-H",
                "127.0.0.1",
                "-P",
                "12345",
                "-s",
                _SERIAL_NUMBER,
                "shell",
                "long_running",
            ],
            capture_output=True,
            capture_error_in_output=True,
            timeout=30.0,
            env=mock.ANY,
        )
        self.assertEqual(
            mock_host_shell_run.call_args_list,
            [expected_call] * adb._DEFAULT_RUN_ATTEMPTS,
        )
        self.assertEqual(
            mock_recover_adb_server.call_count,
            adb._DEFAULT_RUN_ATTEMPTS - 1,
        )

    @mock.patch.object(
        host_shell,
        "run",
        side_effect=errors.HostCmdError("error: device not found"),
        autospec=True,
    )
    def test_run_custom_attempts(self, mock_host_shell_run: mock.Mock) -> None:
        """Test run with custom attempts=1 does not retry and uses full timeout."""
        mock_server = mock.Mock()
        mock_server.host.return_value = "127.0.0.1"
        mock_server.port.return_value = 12345
        self.adb_obj._adb_server = mock_server

        with self.assertRaises(adb_errors.AdbCommandError):
            self.adb_obj.run(["shell", "some_cmd"], timeout=15.0, attempts=1)

        mock_host_shell_run.assert_called_once_with(
            cmd=[
                "/custom/adb",
                "-H",
                "127.0.0.1",
                "-P",
                "12345",
                "-s",
                _SERIAL_NUMBER,
                "shell",
                "some_cmd",
            ],
            capture_output=True,
            capture_error_in_output=True,
            timeout=15.0,
            env=mock.ANY,
        )
        mock_server.restart.assert_not_called()

    @mock.patch.object(host_shell, "run", autospec=True)
    @mock.patch("time.sleep", autospec=True)
    def test_run_retry_success(
        self, mock_sleep: mock.Mock, mock_host_shell_run: mock.Mock
    ) -> None:
        """Test run retries on connection error and succeeds."""
        retriable_errors = [
            "error: device not found",
            "adb: error: failed to read copy response: EOF",
            "adb: error: protocol fault (couldn't read status)",
        ]
        for err_msg in retriable_errors:
            with self.subTest(err_msg=err_msg):
                mock_sleep.reset_mock()
                mock_host_shell_run.reset_mock()
                mock_server = mock.Mock()
                mock_server.host.return_value = "127.0.0.1"
                mock_server.port.return_value = 12345
                self.adb_obj._adb_server = mock_server

                # 1. First attempt: fail with retriable error
                # 2. _recover_adb_server -> _cache_adbd_pid() runs "shell pidof adbd"
                # 3. Second attempt: succeed
                mock_host_shell_run.side_effect = [
                    errors.HostCmdError(err_msg),
                    "5555\n",
                    "success_output",
                ]

                output = self.adb_obj.run(["shell", "some_cmd"])

                self.assertEqual(output, "success_output")
                self.assertEqual(mock_host_shell_run.call_count, 3)
                mock_server.restart.assert_called_once()
                mock_sleep.assert_any_call(10)

    @mock.patch.object(host_shell, "run", autospec=True)
    @mock.patch("time.sleep", autospec=True)
    def test_run_retries_tcp_drop_errors(
        self, mock_sleep: mock.Mock, mock_host_shell_run: mock.Mock
    ) -> None:
        """Test run retries on TCP connection drop error messages."""
        mock_server = mock.Mock()
        mock_server.host.return_value = "127.0.0.1"
        mock_server.port.return_value = 12345
        self.adb_obj._adb_server = mock_server

        for err_msg in (
            "error: closed",
            "error: protocol fault (couldn't read status)",
            "Connection reset by peer",
            "No route to host",
            "Network is unreachable",
            "Connection timed out",
        ):
            with self.subTest(err_msg=err_msg):
                mock_server.reset_mock()
                mock_host_shell_run.reset_mock()
                mock_host_shell_run.side_effect = [
                    errors.HostCmdError(err_msg),
                    "5555\n",
                    "recovered_output",
                ]
                output = self.adb_obj.run(["shell", "some_cmd"])
                self.assertEqual(output, "recovered_output")
                mock_server.restart.assert_called_once()

    @mock.patch.object(host_shell, "run", autospec=True)
    @mock.patch("time.sleep", autospec=True)
    def test_run_retry_fail_max_attempts(
        self, mock_sleep: mock.Mock, mock_host_shell_run: mock.Mock
    ) -> None:
        """Test run retries on connection error but fails after max attempts."""
        # Set a mocked ADB server
        mock_server = mock.Mock()
        mock_server.host.return_value = "127.0.0.1"
        mock_server.port.return_value = 12345
        self.adb_obj._adb_server = mock_server

        # All 3 command attempts fail with "device not found", with 2 _cache_adbd_pid() calls in between
        mock_host_shell_run.side_effect = [
            errors.HostCmdError("error: device not found"),
            errors.HostCmdError("error: device not found"),
            errors.HostCmdError("error: device not found"),
            errors.HostCmdError("error: device not found"),
            errors.HostCmdError("error: device not found"),
        ]

        with self.assertRaises(adb_errors.AdbCommandError):
            self.adb_obj.run(["shell", "some_cmd"])

        self.assertEqual(mock_host_shell_run.call_count, 5)
        self.assertEqual(
            mock_server.restart.call_count, 2
        )  # Restarted on 1st and 2nd failure
        retry_calls = [
            c for c in mock_sleep.call_args_list if c == mock.call(10)
        ]
        self.assertEqual(len(retry_calls), 2)

    @mock.patch.object(host_shell, "run", autospec=True)
    @mock.patch("time.sleep", autospec=True)
    def test_run_retry_offline_reconnect_success(
        self, mock_sleep: mock.Mock, mock_host_shell_run: mock.Mock
    ) -> None:
        """Test run attempts lightweight 'adb reconnect offline' on first offline error."""
        mock_server = mock.Mock()
        mock_server.host.return_value = "127.0.0.1"
        mock_server.port.return_value = 12345
        self.adb_obj._adb_server = mock_server

        # 1. Main command attempt 1 -> fails with "error: device offline"
        # 2. Recovery -> calls "adb reconnect offline"
        # 3. Main command attempt 2 -> succeeds
        mock_host_shell_run.side_effect = [
            errors.HostCmdError("error: device offline"),
            "reconnecting offline",
            "success_output",
        ]

        output = self.adb_obj.run(["shell", "some_cmd"])

        self.assertEqual(output, "success_output")
        self.assertEqual(mock_host_shell_run.call_count, 3)
        self.assertEqual(
            mock_host_shell_run.call_args_list[1].kwargs["cmd"],
            [
                "/custom/adb",
                "-H",
                "127.0.0.1",
                "-P",
                "12345",
                "reconnect",
                "offline",
            ],
        )
        mock_server.restart.assert_not_called()
        mock_sleep.assert_called_once_with(2)

    @mock.patch.object(host_shell, "run", autospec=True)
    @mock.patch("time.sleep", autospec=True)
    def test_run_retry_offline_escalates_to_server_restart(
        self, mock_sleep: mock.Mock, mock_host_shell_run: mock.Mock
    ) -> None:
        """Test run escalates from 'adb reconnect offline' to full server restart on 2nd offline error."""
        mock_server = mock.Mock()
        mock_server.host.return_value = "127.0.0.1"
        mock_server.port.return_value = 12345
        self.adb_obj._adb_server = mock_server

        # 1. Main command attempt 1 -> fails with "error: device offline"
        # 2. Attempt 1 recovery -> "adb reconnect offline"
        # 3. Main command attempt 2 -> still fails with "error: device offline"
        # 4. Attempt 2 recovery -> _recover_adb_server() restarts server & runs _cache_adbd_pid()
        # 5. Main command attempt 3 -> succeeds
        mock_host_shell_run.side_effect = [
            errors.HostCmdError("error: device offline"),
            "reconnecting offline",
            errors.HostCmdError("error: device offline"),
            "5555\n",
            "success_output",
        ]

        output = self.adb_obj.run(["shell", "some_cmd"])

        self.assertEqual(output, "success_output")
        self.assertEqual(mock_host_shell_run.call_count, 5)
        mock_server.restart.assert_called_once()

    @mock.patch.object(host_shell, "run", autospec=True)
    @mock.patch("time.sleep", autospec=True)
    def test_run_retry_offline_reconnect_failure_escalates_to_server_restart(
        self, mock_sleep: mock.Mock, mock_host_shell_run: mock.Mock
    ) -> None:
        """Test run still retries and escalates to server restart when 'adb reconnect offline' fails."""
        mock_server = mock.Mock()
        mock_server.host.return_value = "127.0.0.1"
        mock_server.port.return_value = 12345
        self.adb_obj._adb_server = mock_server

        # 1. Main command attempt 1 -> fails with "error: device offline"
        # 2. Attempt 1 recovery -> "adb reconnect offline" itself fails
        # 3. Main command attempt 2 -> still fails with "error: device offline"
        # 4. Attempt 2 recovery -> _recover_adb_server() restarts server & runs _cache_adbd_pid()
        # 5. Main command attempt 3 -> succeeds
        mock_host_shell_run.side_effect = [
            errors.HostCmdError("error: device offline"),
            errors.HostCmdError("error: reconnect failed"),
            errors.HostCmdError("error: device offline"),
            "5555\n",
            "success_output",
        ]

        output = self.adb_obj.run(["shell", "some_cmd"])

        self.assertEqual(output, "success_output")
        self.assertEqual(mock_host_shell_run.call_count, 5)
        mock_server.restart.assert_called_once()

    def test_run_chains_underlying_error(self) -> None:
        """Test run chains the underlying error as __cause__ of the raised ADB error."""
        cases: list[tuple[Exception, type[adb_errors.AdbError]]] = [
            (errors.HostCmdError("error_text"), adb_errors.AdbCommandError),
            (
                errors.HostCmdError("error: device unauthorized."),
                adb_errors.AdbUnauthorizedError,
            ),
            (
                errors.HoneydewTimeoutError("timed out"),
                adb_errors.AdbTimeoutError,
            ),
        ]
        for underlying_err, expected_err_type in cases:
            with self.subTest(underlying_err=underlying_err):
                with mock.patch.object(
                    host_shell, "run", side_effect=underlying_err, autospec=True
                ):
                    with self.assertRaises(expected_err_type) as context:
                        self.adb_obj.run(["shell", "id"])

                self.assertIs(context.exception.__cause__, underlying_err)

    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_check_connection_chains_underlying_error(
        self, mock_run: mock.Mock
    ) -> None:
        """Test check_connection chains the underlying error as __cause__ of AdbConnectionError."""
        underlying_err = adb_errors.AdbCommandError("wait-for-device failed")
        mock_run.side_effect = underlying_err
        self._check_connection_patcher.stop()
        try:
            with self.assertRaises(adb_errors.AdbConnectionError) as context:
                self.adb_obj.check_connection(timeout=10.0)
        finally:
            self._check_connection_patcher.start()

        self.assertIs(context.exception.__cause__, underlying_err)

    @mock.patch.object(host_shell, "run", autospec=True)
    def test_run_concurrent_calls_return_own_output(
        self, mock_host_shell_run: mock.Mock
    ) -> None:
        """Test concurrent run calls from multiple threads each get their own output."""
        num_threads = 2
        mock_server = mock.Mock()
        mock_server.host.return_value = "127.0.0.1"
        mock_server.port.return_value = 12345
        self.adb_obj._adb_server = mock_server

        # Hold every call until all threads are inside host_shell.run so the
        # calls overlap.
        barrier = threading.Barrier(num_threads)

        def _fake_run(cmd: list[str], **unused_kwargs: Any) -> str:
            barrier.wait(timeout=5)
            return f"output_{cmd[-1]}"

        mock_host_shell_run.side_effect = _fake_run

        with concurrent.futures.ThreadPoolExecutor(num_threads) as executor:
            futures = {
                i: executor.submit(self.adb_obj.run, ["shell", "echo", str(i)])
                for i in range(num_threads)
            }
            outputs = {i: f.result(timeout=10) for i, f in futures.items()}

        self.assertEqual(
            outputs, {i: f"output_{i}" for i in range(num_threads)}
        )
        self.assertEqual(mock_host_shell_run.call_count, num_threads)
        mock_server.restart.assert_not_called()
        self.assertEqual(
            mock_server.reset_restart_count.call_count, num_threads
        )

    @mock.patch.object(host_shell, "run", autospec=True)
    @mock.patch("time.sleep", autospec=True)
    def test_recover_adb_server_with_ffx_and_root_restoration(
        self, mock_sleep: mock.Mock, mock_host_shell_run: mock.Mock
    ) -> None:
        """Test _recover_adb_server kills adbd via ffx, restarts server, restores root, and re-caches PID."""
        mock_server = mock.Mock()
        mock_server.host.return_value = "127.0.0.1"
        mock_server.port.return_value = 12345
        mock_ffx = mock.Mock()
        self.adb_obj._adb_server = mock_server
        self.adb_obj._ffx = mock_ffx
        self.adb_obj._is_root = True

        # Cache initial adbd PID
        mock_host_shell_run.return_value = "4321\n"
        self.adb_obj._cache_adbd_pid()
        self.assertEqual(self.adb_obj._cached_adbd_pid, "4321")

        # Now run a command that fails on attempt 1 with a connection error:
        # 1. Main command attempt 1 -> fails with "error: device not found"
        # 2. _recover_adb_server -> runs ["root"] then ["wait-for-device"]
        # 3. _recover_adb_server -> calls _cache_adbd_pid() which runs ["shell", "pidof", "adbd"]
        # 4. Main command attempt 2 -> succeeds
        mock_host_shell_run.side_effect = [
            errors.HostCmdError("error: device not found"),
            "restarting adbd as root",
            "",
            "8765\n",
            "cmd_output",
        ]

        output = self.adb_obj.run(["shell", "id"])

        self.assertEqual(output, "cmd_output")
        mock_ffx.run.assert_called_once_with(
            ["starnix", "kill", "-p", "4321", "-s", "9"],
            timeout=10.0,
        )
        mock_server.restart.assert_called_once()
        self.assertTrue(self.adb_obj._is_root)
        self.assertEqual(self.adb_obj._cached_adbd_pid, "8765")
        self.mock_usb.before_disconnect.assert_called_once_with()
        self.mock_usb.after_connect.assert_called_once_with()

    @mock.patch.object(host_shell, "run", autospec=True)
    @mock.patch("time.sleep", autospec=True)
    def test_recover_adb_server_logs_warning_when_usb_after_connect_fails(
        self, mock_sleep: mock.Mock, mock_host_shell_run: mock.Mock
    ) -> None:
        """Test _recover_adb_server catches HoneydewError from usb.after_connect()."""
        mock_server = mock.Mock()
        self.adb_obj._adb_server = mock_server
        self.mock_usb.after_connect.side_effect = errors.FuchsiaDeviceError(
            "USB reconnect failed"
        )
        mock_host_shell_run.return_value = "8765\n"

        self.adb_obj._recover_adb_server()

        self.mock_usb.before_disconnect.assert_called_once_with()
        self.mock_usb.after_connect.assert_called_once_with()
        self.assertEqual(self.adb_obj._cached_adbd_pid, "8765")
        self.assertFalse(self.adb_obj._is_recovering)

    @mock.patch.object(host_shell, "run", autospec=True)
    @mock.patch("time.sleep", autospec=True)
    def test_recover_adb_server_tcp_serial_reconnects(
        self, mock_sleep: mock.Mock, mock_host_shell_run: mock.Mock
    ) -> None:
        """Test _recover_adb_server reconnects over TCP after server restart and root."""
        self.adb_obj._serial_number = _TCP_SERIAL_NUMBER
        mock_server = mock.create_autospec(adb_server.AdbServer, instance=True)
        mock_server.host.return_value = "127.0.0.1"
        mock_server.port.return_value = 12345
        self.adb_obj._adb_server = mock_server
        self.adb_obj._is_root = True

        mock_host_shell_run.side_effect = [
            f"connected to {_TCP_SERIAL_NUMBER}\n",
            "restarting adbd as root",
            f"connected to {_TCP_SERIAL_NUMBER}\n",
            "",
            "8765\n",
        ]

        self.adb_obj._recover_adb_server()

        mock_server.restart.assert_called_once()
        self.assertEqual(self.adb_obj._cached_adbd_pid, "8765")
        connect_calls = [
            c
            for c in mock_host_shell_run.call_args_list
            if c.kwargs["cmd"][-2:] == ["connect", _TCP_SERIAL_NUMBER]
        ]
        self.assertEqual(len(connect_calls), 2)

    @mock.patch.object(adb.Adb, "_cache_adbd_pid", autospec=True)
    @mock.patch.object(adb.Adb, "_connect_tcp", autospec=True)
    @mock.patch.object(host_shell, "run", autospec=True)
    @mock.patch("time.sleep", autospec=True)
    def test_recover_adb_server_tcp_serial_connect_failure_and_unauthorized(
        self,
        mock_sleep: mock.Mock,
        mock_host_shell_run: mock.Mock,
        mock_connect_tcp: mock.Mock,
        mock_cache_pid: mock.Mock,
    ) -> None:
        """Test _recover_adb_server propagates AdbConnectionError and AdbUnauthorizedError on TCP connect failure."""
        self.adb_obj._serial_number = _TCP_SERIAL_NUMBER
        mock_server = mock.create_autospec(adb_server.AdbServer, instance=True)
        self.adb_obj._adb_server = mock_server
        self.adb_obj._is_root = True

        # 1. Connection error -> propagates immediately without calling root or _cache_adbd_pid
        mock_connect_tcp.side_effect = adb_errors.AdbConnectionError(
            "connect timed out"
        )
        with self.assertRaises(adb_errors.AdbConnectionError):
            self.adb_obj._recover_adb_server()
        mock_host_shell_run.assert_not_called()
        mock_cache_pid.assert_not_called()

        # 2. Unauthorized error -> propagates immediately
        mock_connect_tcp.side_effect = adb_errors.AdbUnauthorizedError(
            "unauthorized"
        )
        with self.assertRaises(adb_errors.AdbUnauthorizedError):
            self.adb_obj._recover_adb_server()
        mock_host_shell_run.assert_not_called()
        mock_cache_pid.assert_not_called()

    @mock.patch.object(adb.Adb, "_connect_tcp", autospec=True)
    @mock.patch.object(host_shell, "run", autospec=True)
    def test_run_retries_tcp_drop_without_isolated_server(
        self,
        mock_host_shell_run: mock.Mock,
        mock_connect_tcp: mock.Mock,
    ) -> None:
        """Test run reconnects over TCP and retries on drop even when _adb_server is None."""
        self.adb_obj._serial_number = _TCP_SERIAL_NUMBER
        self.adb_obj._adb_server = None

        mock_host_shell_run.side_effect = [
            errors.HostCmdError(
                f"error: device '{_TCP_SERIAL_NUMBER}' not found"
            ),
            "recovered_output",
        ]

        output = self.adb_obj.run(["shell", "id"])
        self.assertEqual(output, "recovered_output")
        mock_connect_tcp.assert_called_once_with(
            self.adb_obj, timeout=adb._TCP_RECONNECT_TIMEOUT_SECS
        )
        self.assertEqual(mock_host_shell_run.call_count, 2)

        # Verify AdbUnauthorizedError during TCP reconnect propagates immediately
        mock_connect_tcp.reset_mock()
        mock_connect_tcp.side_effect = adb_errors.AdbUnauthorizedError(
            "unauthorized"
        )
        mock_host_shell_run.side_effect = errors.HostCmdError("error: closed")
        with self.assertRaises(adb_errors.AdbUnauthorizedError):
            self.adb_obj.run(["shell", "id"])

    @mock.patch.object(host_shell, "run", autospec=True)
    def test_reconnect_offline_tcp_serial(
        self, mock_host_shell_run: mock.Mock
    ) -> None:
        """Test _reconnect_offline runs adb connect after reconnect offline for TCP."""
        self.adb_obj._serial_number = _TCP_SERIAL_NUMBER
        mock_host_shell_run.side_effect = [
            "reconnecting offline",
            f"connected to {_TCP_SERIAL_NUMBER}\n",
        ]

        self.adb_obj._reconnect_offline()

        self.assertEqual(mock_host_shell_run.call_count, 2)
        self.assertEqual(
            mock_host_shell_run.call_args_list[0].kwargs["cmd"],
            ["/custom/adb", "reconnect", "offline"],
        )
        self.assertEqual(
            mock_host_shell_run.call_args_list[1].kwargs["cmd"],
            ["/custom/adb", "connect", _TCP_SERIAL_NUMBER],
        )

    @mock.patch(
        "time.time",
        side_effect=[100.0, 102.0, 104.0, 105.0, 105.0, 110.0, 112.0, 115.0],
        autospec=True,
    )
    @mock.patch.object(adb.Adb, "_connect_tcp", autospec=True)
    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_root_and_unroot_tcp_serial(
        self,
        mock_run: mock.Mock,
        mock_connect_tcp: mock.Mock,
        mock_time: mock.Mock,
    ) -> None:
        """Test root and unroot reconnect over TCP before wait-for-device."""
        self.adb_obj._serial_number = _TCP_SERIAL_NUMBER
        # Report adbd as root (UID 0) when root() verifies it.
        mock_run.side_effect = lambda _, cmd, **kwargs: (
            "0" if cmd[0] == "exec-out" else ""
        )

        self.adb_obj.root(timeout=10.0)
        self.adb_obj.unroot(timeout=10.0)

        mock_connect_tcp.assert_has_calls(
            [
                mock.call(self.adb_obj, timeout=8.0),
                mock.call(self.adb_obj, timeout=8.0),
            ]
        )
        mock_run.assert_has_calls(
            [
                mock.call(
                    self.adb_obj,
                    ["root"],
                    timeout=10.0 / adb._DEFAULT_RUN_ATTEMPTS,
                    attempts=adb._DEFAULT_RUN_ATTEMPTS,
                ),
                mock.call(
                    self.adb_obj,
                    ["wait-for-device"],
                    timeout=6.0 / adb._DEFAULT_RUN_ATTEMPTS,
                    attempts=adb._DEFAULT_RUN_ATTEMPTS,
                    log_devices_on_failure=True,
                ),
                mock.call(
                    self.adb_obj,
                    ["exec-out", "id", "-u"],
                    timeout=5.0,
                    attempts=1,
                ),
                mock.call(
                    self.adb_obj,
                    ["unroot"],
                    timeout=10.0 / adb._DEFAULT_RUN_ATTEMPTS,
                    attempts=adb._DEFAULT_RUN_ATTEMPTS,
                ),
                mock.call(
                    self.adb_obj,
                    ["wait-for-device"],
                    timeout=5.0 / adb._DEFAULT_RUN_ATTEMPTS,
                    attempts=adb._DEFAULT_RUN_ATTEMPTS,
                    log_devices_on_failure=True,
                ),
            ]
        )

    def test_close(self) -> None:
        """Test close stops the adb server if running and drops the reference."""
        mock_server = mock.Mock()
        self.adb_obj._adb_server = mock_server
        self.adb_obj.close()
        mock_server.stop.assert_called_once()
        self.assertIsNone(self.adb_obj._adb_server)

    def test_close_is_idempotent(self) -> None:
        """Test calling close multiple times only cleans up resources once."""
        mock_server = mock.Mock()
        mock_temp_dir = mock.Mock()
        self.adb_obj._adb_server = mock_server
        self.adb_obj._temp_vendor_keys_dir = mock_temp_dir

        self.adb_obj.close()
        self.adb_obj.close()

        mock_server.stop.assert_called_once()
        mock_temp_dir.cleanup.assert_called_once()

    @mock.patch("atexit.unregister", autospec=True)
    def test_close_unregisters_atexit_handler(
        self, mock_unregister: mock.Mock
    ) -> None:
        """Test close unregisters its atexit handler."""
        self.adb_obj.close()
        mock_unregister.assert_called_once_with(self.adb_obj.close)

    def test_close_cleans_up_vendor_keys(self) -> None:
        """Test close cleans up temp vendor keys dir if present."""
        mock_temp_dir = mock.Mock()
        self.adb_obj._temp_vendor_keys_dir = mock_temp_dir
        self.adb_obj.close()
        mock_temp_dir.cleanup.assert_called_once()
        self.assertIsNone(self.adb_obj._temp_vendor_keys_dir)

    def test_cached_root_state_default(self) -> None:
        """Test the cached root state is False by default."""
        self.assertFalse(self.adb_obj._is_root)

    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_verify_root_updates_cached_state(
        self, mock_run: mock.Mock
    ) -> None:
        """Test verify_root queries adbd's UID via exec-out and updates _is_root."""
        for output, expected in (("0\n", True), ("2000\n", False)):
            with self.subTest(output=output):
                mock_run.reset_mock()
                mock_run.return_value = output
                self.adb_obj._is_root = not expected

                self.assertEqual(
                    self.adb_obj.verify_root(timeout=5.0), expected
                )

                self.assertEqual(self.adb_obj._is_root, expected)
                mock_run.assert_called_once_with(
                    self.adb_obj,
                    ["exec-out", "id", "-u"],
                    timeout=5.0,
                    attempts=1,
                )

        mock_run.reset_mock()
        mock_run.return_value = "0\n"
        self.assertTrue(self.adb_obj.verify_root(timeout=None))
        mock_run.assert_called_once_with(
            self.adb_obj,
            ["exec-out", "id", "-u"],
            timeout=None,
            attempts=1,
        )

    @mock.patch("time.sleep", autospec=True)
    @mock.patch(
        "time.time",
        side_effect=[100.0, 101.8, 102.0, 200.0, 201.8, 202.0],
        autospec=True,
    )
    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_verify_root_retries_once(
        self, mock_run: mock.Mock, mock_time: mock.Mock, mock_sleep: mock.Mock
    ) -> None:
        """Test verify_root retries once on command error or empty output, within remaining timeout."""
        for first_failure in (adb_errors.AdbCommandError("closed"), ""):
            with self.subTest(first_failure=first_failure):
                mock_run.reset_mock()
                mock_sleep.reset_mock()
                mock_run.side_effect = [first_failure, "0"]

                self.assertTrue(self.adb_obj.verify_root(timeout=5.0))

                self.assertEqual(
                    mock_run.call_args_list[1].kwargs["timeout"], 3.0
                )
                mock_sleep.assert_called_once_with(
                    adb._ROOT_CHECK_RETRY_DELAY_SECS
                )

    @mock.patch("time.sleep", autospec=True)
    @mock.patch("time.time", side_effect=[100.0, 105.0], autospec=True)
    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_verify_root_timeout_expired_before_retry_sleep(
        self, mock_run: mock.Mock, mock_time: mock.Mock, mock_sleep: mock.Mock
    ) -> None:
        """Test verify_root checks remaining budget before sleeping on retry."""
        mock_run.side_effect = adb_errors.AdbCommandError("closed")

        with self.assertRaises(adb_errors.AdbTimeoutError):
            self.adb_obj.verify_root(timeout=5.0)

        mock_run.assert_called_once()
        mock_sleep.assert_not_called()

    @mock.patch("time.sleep", autospec=True)
    @mock.patch("time.time", side_effect=[100.0, 104.9], autospec=True)
    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_verify_root_raises_original_error_without_budget_to_retry(
        self, mock_run: mock.Mock, mock_time: mock.Mock, mock_sleep: mock.Mock
    ) -> None:
        """Test verify_root re-raises the original error if the budget can't cover the retry delay."""
        mock_run.side_effect = adb_errors.AdbCommandError("closed")

        with self.assertRaisesRegex(adb_errors.AdbCommandError, "closed"):
            self.adb_obj.verify_root(timeout=5.0)

        mock_run.assert_called_once()
        mock_sleep.assert_not_called()

    @mock.patch("time.sleep", autospec=True)
    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_verify_root_does_not_retry_unauthorized(
        self, mock_run: mock.Mock, mock_sleep: mock.Mock
    ) -> None:
        """Test verify_root re-raises AdbUnauthorizedError without retrying."""
        mock_run.side_effect = adb_errors.AdbUnauthorizedError("unauthorized")

        with self.assertRaises(adb_errors.AdbUnauthorizedError):
            self.adb_obj.verify_root(timeout=0.01)

        mock_run.assert_called_once()
        mock_sleep.assert_not_called()

    @mock.patch("time.sleep", autospec=True)
    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_verify_root_raises_after_retry(
        self, mock_run: mock.Mock, mock_sleep: mock.Mock
    ) -> None:
        """Test verify_root raises if the retry also fails."""
        mock_run.side_effect = adb_errors.AdbCommandError("offline")

        with self.assertRaises(adb_errors.AdbCommandError):
            self.adb_obj.verify_root()

        self.assertEqual(mock_run.call_count, 2)

    @mock.patch("time.sleep", autospec=True)
    @mock.patch.object(adb.Adb, "run", return_value="oops", autospec=True)
    def test_verify_root_raises_on_unexpected_output(
        self, mock_run: mock.Mock, mock_sleep: mock.Mock
    ) -> None:
        """Test verify_root retries once and raises AdbCommandError on non-integer output."""
        with self.assertRaisesRegex(adb_errors.AdbCommandError, "oops"):
            self.adb_obj.verify_root()

        self.assertEqual(mock_run.call_count, 2)
        mock_sleep.assert_called_once_with(adb._ROOT_CHECK_RETRY_DELAY_SECS)

    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_is_rootable(self, mock_run: mock.Mock) -> None:
        """Test is_rootable checks ro.debuggable via exec-out, parsing the last line."""
        for value, expected in (
            ("1\n", True),
            ("0\n", False),
            ("\n", False),
            ("* daemon started successfully\n1\n", True),
            ("* daemon started successfully\n0\n", False),
        ):
            with self.subTest(value=value):
                mock_run.return_value = value
                self.assertEqual(self.adb_obj.is_rootable(), expected)

        mock_run.assert_called_with(
            self.adb_obj,
            ["exec-out", "getprop", "ro.debuggable"],
            timeout=adb._ROOT_CHECK_TIMEOUT_SECS,
            attempts=1,
        )

        self.adb_obj.is_rootable(timeout=2.0)
        mock_run.assert_called_with(
            self.adb_obj,
            ["exec-out", "getprop", "ro.debuggable"],
            timeout=2.0,
            attempts=1,
        )

        self.adb_obj.is_rootable(timeout=None)
        mock_run.assert_called_with(
            self.adb_obj,
            ["exec-out", "getprop", "ro.debuggable"],
            timeout=None,
            attempts=1,
        )

    @mock.patch("time.sleep", autospec=True)
    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_verify_root_does_not_retry_timeout(
        self, mock_run: mock.Mock, mock_sleep: mock.Mock
    ) -> None:
        """Test verify_root fails fast on a timeout instead of retrying."""
        mock_run.side_effect = adb_errors.AdbTimeoutError("timed out")

        with self.assertRaises(adb_errors.AdbTimeoutError):
            self.adb_obj.verify_root()

        mock_run.assert_called_once_with(
            self.adb_obj,
            ["exec-out", "id", "-u"],
            timeout=adb._ROOT_CHECK_TIMEOUT_SECS,
            attempts=1,
        )
        mock_sleep.assert_not_called()

    @mock.patch("time.sleep", autospec=True)
    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_verify_root_parses_last_line(
        self, mock_run: mock.Mock, mock_sleep: mock.Mock
    ) -> None:
        """Test verify_root ignores adb messages before the UID, and empty output."""
        mock_run.return_value = (
            "* daemon not running; starting now at tcp:5037\n"
            "* daemon started successfully\n"
            "0\n"
        )
        self.assertTrue(self.adb_obj.verify_root())

        mock_run.return_value = "  \n"
        with self.assertRaises(adb_errors.AdbCommandError):
            self.adb_obj.verify_root()

    @mock.patch(
        "time.time", side_effect=[100.0, 103.0, 104.0, 104.0], autospec=True
    )
    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_root(self, mock_run: mock.Mock, mock_time: mock.Mock) -> None:
        """Test root runs root and wait-for-device, then verifies root with the remaining timeout."""
        mock_run.side_effect = ["", "", "0\n"]

        self.adb_obj.root(timeout=10.0)

        self.assertEqual(
            mock_run.call_args_list,
            [
                mock.call(
                    self.adb_obj,
                    ["root"],
                    timeout=10.0 / adb._DEFAULT_RUN_ATTEMPTS,
                    attempts=adb._DEFAULT_RUN_ATTEMPTS,
                ),
                mock.call(
                    self.adb_obj,
                    ["wait-for-device"],
                    timeout=7.0 / adb._DEFAULT_RUN_ATTEMPTS,
                    attempts=adb._DEFAULT_RUN_ATTEMPTS,
                    log_devices_on_failure=True,
                ),
                mock.call(
                    self.adb_obj,
                    ["exec-out", "id", "-u"],
                    timeout=6.0,
                    attempts=1,
                ),
            ],
        )
        self.assertTrue(self.adb_obj._is_root)
        self.mock_usb.before_disconnect.assert_called_once_with()
        self.mock_usb.after_connect.assert_called_once_with()

    @mock.patch(
        "time.time",
        side_effect=[100.0, 103.0, 104.0, 104.0, 200.0, 204.0],
        autospec=True,
    )
    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_root_without_usb(
        self, mock_run: mock.Mock, mock_time: mock.Mock
    ) -> None:
        """Test root and unroot succeed when usb is None."""
        mock_run.side_effect = ["", "", "0\n", "", ""]
        self.adb_obj._usb = None
        self.adb_obj.root(timeout=10.0)
        self.assertTrue(self.adb_obj._is_root)

        self.adb_obj.unroot(timeout=10.0)
        self.assertFalse(self.adb_obj._is_root)
        self.mock_usb.before_disconnect.assert_not_called()
        self.mock_usb.after_connect.assert_not_called()

    @mock.patch.object(adb.Adb, "is_rootable", autospec=True)
    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_root_raises_when_root_not_granted(
        self, mock_run: mock.Mock, mock_is_rootable: mock.Mock
    ) -> None:
        """Test root raises if adbd is not root afterwards, reporting output and is_rootable()."""
        for root_out, is_rootable, expected_regex in (
            (
                "adbd cannot run as root in production builds\n",
                False,
                r"did not make adbd run as root.*cannot run as root.*`ro\.debuggable` is not 1",
            ),
            (
                "",
                True,
                "did not make adbd run as root.*although the build is debuggable",
            ),
            (
                "",
                adb_errors.AdbCommandError("offline"),
                "did not make adbd run as root.*could not check",
            ),
        ):
            with self.subTest(expected_regex=expected_regex):
                mock_run.reset_mock()
                mock_is_rootable.reset_mock()
                self.mock_usb.reset_mock()
                mock_run.side_effect = [root_out, "", "2000\n"]
                # Returns the value, or raises it if it is an exception.
                mock_is_rootable.side_effect = [is_rootable]

                with self.assertRaisesRegex(
                    adb_errors.AdbCommandError, expected_regex
                ):
                    self.adb_obj.root()

                self.assertEqual(
                    mock_run.call_args_list[-1],
                    mock.call(
                        self.adb_obj,
                        ["exec-out", "id", "-u"],
                        timeout=None,
                        attempts=1,
                    ),
                )
                mock_is_rootable.assert_called_once_with(
                    self.adb_obj, timeout=adb._ROOT_CHECK_TIMEOUT_SECS
                )
                self.assertFalse(self.adb_obj._is_root)
                self.mock_usb.before_disconnect.assert_called_once_with()
                self.mock_usb.after_connect.assert_not_called()

    @mock.patch.object(
        adb.Adb, "is_rootable", return_value=False, autospec=True
    )
    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_root_caps_is_rootable_timeout_to_remaining_budget(
        self,
        mock_run: mock.Mock,
        mock_is_rootable: mock.Mock,
    ) -> None:
        """Test root passes the remaining timeout, capped at 10s, to is_rootable on failure."""
        for timeout, last_time, expected_check_timeout in (
            (4.0, 102.5, 1.5),
            (30.0, 115.0, adb._ROOT_CHECK_TIMEOUT_SECS),
        ):
            with self.subTest(timeout=timeout):
                mock_run.reset_mock()
                mock_is_rootable.reset_mock()
                mock_run.side_effect = ["", "", "2000\n"]

                with mock.patch(
                    "time.time",
                    side_effect=[100.0, 101.0, 102.0, 102.0, last_time],
                    autospec=True,
                ):
                    with self.assertRaises(adb_errors.AdbCommandError):
                        self.adb_obj.root(timeout=timeout)

                mock_is_rootable.assert_called_once_with(
                    self.adb_obj, timeout=expected_check_timeout
                )

    @mock.patch("time.time", side_effect=[100.0, 110.0], autospec=True)
    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_root_timeout_expired(
        self, mock_run: mock.Mock, mock_time: mock.Mock
    ) -> None:
        """Test root raises AdbTimeoutError if timeout expires during root command."""
        with self.assertRaisesRegex(adb_errors.AdbTimeoutError, "root"):
            self.adb_obj.root(timeout=10.0)

        mock_run.assert_called_once_with(
            self.adb_obj,
            ["root"],
            timeout=10.0 / adb._DEFAULT_RUN_ATTEMPTS,
            attempts=adb._DEFAULT_RUN_ATTEMPTS,
        )
        self.assertFalse(self.adb_obj._is_root)
        self.mock_usb.before_disconnect.assert_called_once_with()
        # The reconnect hook only runs on success, so it cannot mask the
        # original `adb root` failure.
        self.mock_usb.after_connect.assert_not_called()

    @mock.patch("time.time", side_effect=[100.0, 104.0], autospec=True)
    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_unroot(self, mock_run: mock.Mock, mock_time: mock.Mock) -> None:
        """Test unroot runs unroot and wait-for-device commands, deducting elapsed timeout."""
        self.adb_obj._is_root = True

        self.adb_obj.unroot(timeout=10.0)

        mock_run.assert_has_calls(
            [
                mock.call(
                    self.adb_obj,
                    ["unroot"],
                    timeout=10.0 / adb._DEFAULT_RUN_ATTEMPTS,
                    attempts=adb._DEFAULT_RUN_ATTEMPTS,
                ),
                mock.call(
                    self.adb_obj,
                    ["wait-for-device"],
                    timeout=6.0 / adb._DEFAULT_RUN_ATTEMPTS,
                    attempts=adb._DEFAULT_RUN_ATTEMPTS,
                    log_devices_on_failure=True,
                ),
            ]
        )
        self.assertFalse(self.adb_obj._is_root)
        self.mock_usb.before_disconnect.assert_called_once_with()
        self.mock_usb.after_connect.assert_called_once_with()

    @mock.patch("time.time", side_effect=[100.0, 110.0], autospec=True)
    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_unroot_timeout_expired(
        self, mock_run: mock.Mock, mock_time: mock.Mock
    ) -> None:
        """Test unroot raises AdbTimeoutError if timeout expires during unroot command."""
        self.adb_obj._is_root = True

        with self.assertRaisesRegex(adb_errors.AdbTimeoutError, "unroot"):
            self.adb_obj.unroot(timeout=10.0)

        mock_run.assert_called_once_with(
            self.adb_obj,
            ["unroot"],
            timeout=10.0 / adb._DEFAULT_RUN_ATTEMPTS,
            attempts=adb._DEFAULT_RUN_ATTEMPTS,
        )
        self.assertTrue(self.adb_obj._is_root)
        self.mock_usb.before_disconnect.assert_called_once_with()
        # The reconnect hook only runs on success, so it cannot mask the
        # original `adb unroot` failure.
        self.mock_usb.after_connect.assert_not_called()

    @mock.patch("time.time", side_effect=[100.0, 102.0], autospec=True)
    @mock.patch.object(
        adb.Adb, "verify_root", return_value=False, autospec=True
    )
    @mock.patch.object(adb.Adb, "unroot", autospec=True)
    @mock.patch.object(adb.Adb, "root", autospec=True)
    def test_use_adb_root_when_not_root(
        self,
        mock_root: mock.Mock,
        mock_unroot: mock.Mock,
        mock_verify_root: mock.Mock,
        mock_time: mock.Mock,
    ) -> None:
        """Test use_adb_root ignores stale _is_root, roots with remaining time, and unroots on exit."""
        self.adb_obj._is_root = True

        with self.adb_obj.use_adb_root(timeout=5.0):
            mock_verify_root.assert_called_once_with(self.adb_obj, timeout=5.0)
            mock_root.assert_called_once_with(
                self.adb_obj, timeout=3.0, attempts=adb._DEFAULT_RUN_ATTEMPTS
            )
            mock_unroot.assert_not_called()

        mock_unroot.assert_called_once_with(
            self.adb_obj, timeout=5.0, attempts=adb._DEFAULT_RUN_ATTEMPTS
        )

    @mock.patch(
        "time.time",
        side_effect=[100.0, 101.0, 200.0, 300.0, 400.0],
        autospec=True,
    )
    @mock.patch.object(adb.Adb, "verify_root", autospec=True)
    @mock.patch.object(adb.Adb, "unroot", autospec=True)
    @mock.patch.object(adb.Adb, "root", autospec=True)
    def test_use_adb_root_falls_through_on_verify_command_error(
        self,
        mock_root: mock.Mock,
        mock_unroot: mock.Mock,
        mock_verify_root: mock.Mock,
        mock_time: mock.Mock,
    ) -> None:
        """Test use_adb_root falls through to root() only on AdbCommandError when attempts > 1."""
        mock_verify_root.side_effect = adb_errors.AdbCommandError("closed")

        with self.adb_obj.use_adb_root(timeout=5.0, attempts=2):
            mock_root.assert_called_once_with(
                self.adb_obj, timeout=4.0, attempts=2
            )

        mock_unroot.assert_called_once_with(
            self.adb_obj, timeout=5.0, attempts=2
        )

        mock_root.reset_mock()
        with self.assertRaises(adb_errors.AdbCommandError):
            with self.adb_obj.use_adb_root(timeout=5.0, attempts=1):
                pass
        mock_root.assert_not_called()

        mock_verify_root.side_effect = adb_errors.AdbUnauthorizedError(
            "unauthorized"
        )
        with self.assertRaises(adb_errors.AdbUnauthorizedError):
            with self.adb_obj.use_adb_root(timeout=5.0, attempts=2):
                pass
        mock_root.assert_not_called()

        mock_verify_root.side_effect = adb_errors.AdbTimeoutError("timed out")
        with self.assertRaises(adb_errors.AdbTimeoutError):
            with self.adb_obj.use_adb_root(timeout=5.0, attempts=2):
                pass
        mock_root.assert_not_called()

    @mock.patch.object(adb.Adb, "verify_root", return_value=True, autospec=True)
    @mock.patch.object(adb.Adb, "unroot", autospec=True)
    @mock.patch.object(adb.Adb, "root", autospec=True)
    def test_use_adb_root_when_already_root(
        self,
        mock_root: mock.Mock,
        mock_unroot: mock.Mock,
        mock_verify_root: mock.Mock,
    ) -> None:
        """Test use_adb_root does not root or unroot when adbd is already root."""
        with self.adb_obj.use_adb_root(timeout=5.0):
            mock_verify_root.assert_called_once_with(self.adb_obj, timeout=5.0)
            mock_root.assert_not_called()
            mock_unroot.assert_not_called()

        mock_root.assert_not_called()
        mock_unroot.assert_not_called()

    @mock.patch("time.time", return_value=100.0, autospec=True)
    @mock.patch.object(
        adb.Adb, "verify_root", return_value=False, autospec=True
    )
    @mock.patch.object(adb.Adb, "unroot", autospec=True)
    @mock.patch.object(adb.Adb, "root", autospec=True)
    def test_use_adb_root_async_context(
        self,
        mock_root: mock.Mock,
        mock_unroot: mock.Mock,
        mock_verify_root: mock.Mock,
        mock_time: mock.Mock,
    ) -> None:
        """Test use_adb_root works as an async context manager."""
        self.assertFalse(self.adb_obj._is_root)

        async def run_async() -> None:
            async with self.adb_obj.use_adb_root(timeout=5.0):
                mock_root.assert_called_once_with(
                    self.adb_obj,
                    timeout=5.0,
                    attempts=adb._DEFAULT_RUN_ATTEMPTS,
                )
                mock_unroot.assert_not_called()

        asyncio.run(run_async())
        mock_unroot.assert_called_once_with(
            self.adb_obj, timeout=5.0, attempts=adb._DEFAULT_RUN_ATTEMPTS
        )

    @mock.patch("time.time", return_value=100.0, autospec=True)
    @mock.patch.object(
        adb.Adb, "verify_root", return_value=False, autospec=True
    )
    @mock.patch.object(adb.Adb, "unroot", autospec=True)
    @mock.patch.object(adb.Adb, "root", autospec=True)
    def test_use_adb_root_concurrent_overlapping(
        self,
        mock_root: mock.Mock,
        mock_unroot: mock.Mock,
        mock_verify_root: mock.Mock,
        mock_time: mock.Mock,
    ) -> None:
        """Test overlapping use_adb_root contexts only unroot when last task exits."""
        ctx_a = self.adb_obj.use_adb_root(timeout=5.0)
        ctx_b = self.adb_obj.use_adb_root(timeout=5.0)

        # Task A enters -> verifies once and roots device (depth 1)
        ctx_a.__enter__()
        mock_verify_root.assert_called_once_with(self.adb_obj, timeout=5.0)
        mock_root.assert_called_once_with(
            self.adb_obj, timeout=5.0, attempts=adb._DEFAULT_RUN_ATTEMPTS
        )
        mock_unroot.assert_not_called()

        # Task B enters while A is active -> does not re-verify or re-root (depth 2)
        ctx_b.__enter__()
        mock_verify_root.assert_called_once()
        mock_root.assert_called_once()
        mock_unroot.assert_not_called()

        # Task A exits first -> must NOT unroot because Task B is still active (depth 1)
        ctx_a.__exit__(None, None, None)
        mock_unroot.assert_not_called()

        # Task B exits -> unroots device as depth returns to 0
        ctx_b.__exit__(None, None, None)
        mock_unroot.assert_called_once_with(
            self.adb_obj, timeout=5.0, attempts=adb._DEFAULT_RUN_ATTEMPTS
        )

    @mock.patch.object(adb.Adb, "su_unroot", autospec=True)
    @mock.patch.object(adb.Adb, "su_root", autospec=True)
    def test_use_su_root_when_not_root(
        self, mock_su_root: mock.Mock, mock_su_unroot: mock.Mock
    ) -> None:
        """Test use_su_root roots on enter and unroots on exit when not root initially."""
        self.assertFalse(self.adb_obj.is_su_root)

        with self.adb_obj.use_su_root(timeout=5.0):
            mock_su_root.assert_called_once_with(
                self.adb_obj, timeout=5.0, attempts=adb._DEFAULT_RUN_ATTEMPTS
            )
            mock_su_unroot.assert_not_called()

        mock_su_unroot.assert_called_once_with(
            self.adb_obj, timeout=5.0, attempts=adb._DEFAULT_RUN_ATTEMPTS
        )

    @mock.patch.object(adb.Adb, "su_unroot", autospec=True)
    @mock.patch.object(adb.Adb, "su_root", autospec=True)
    def test_use_su_root_when_already_root(
        self, mock_su_root: mock.Mock, mock_su_unroot: mock.Mock
    ) -> None:
        """Test use_su_root does not root or unroot when already root."""
        self.adb_obj._is_su_root = True

        with self.adb_obj.use_su_root(timeout=5.0):
            mock_su_root.assert_not_called()
            mock_su_unroot.assert_not_called()

        mock_su_root.assert_not_called()
        mock_su_unroot.assert_not_called()

    def test_su_root_and_su_unroot(self) -> None:
        """Test su_root and su_unroot toggle su root injection in _build_adb_cmd."""
        cmd = ["shell", "setprop", "foo", "bar"]
        self.assertFalse(self.adb_obj.is_su_root)

        self.adb_obj.su_root(timeout=10.0)
        self.assertTrue(self.adb_obj.is_su_root)
        self.assertEqual(
            self.adb_obj._build_adb_cmd(cmd),
            [
                "/custom/adb",
                "-s",
                _SERIAL_NUMBER,
                "shell",
                "su",
                "root",
                "setprop",
                "foo",
                "bar",
            ],
        )
        # Ensure original cmd list was not mutated
        self.assertEqual(cmd, ["shell", "setprop", "foo", "bar"])

        self.adb_obj.su_unroot(timeout=10.0)
        self.assertFalse(self.adb_obj.is_su_root)
        self.assertEqual(
            self.adb_obj._build_adb_cmd(cmd),
            [
                "/custom/adb",
                "-s",
                _SERIAL_NUMBER,
                "shell",
                "setprop",
                "foo",
                "bar",
            ],
        )

    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_setprop(self, mock_run: mock.Mock) -> None:
        """Test setprop runs adb shell setprop command."""
        self.adb_obj.setprop("persist.test.prop", "val123", timeout=5.0)

        mock_run.assert_called_once_with(
            self.adb_obj,
            ["shell", "setprop", "persist.test.prop", "val123"],
            timeout=5.0,
            attempts=adb._DEFAULT_RUN_ATTEMPTS,
        )

    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_getprop(self, mock_run: mock.Mock) -> None:
        """Test getprop runs adb shell getprop command and strips result."""
        mock_run.return_value = "  val123 \n"

        result = self.adb_obj.getprop("persist.test.prop", timeout=5.0)

        self.assertEqual(result, "val123")
        mock_run.assert_called_once_with(
            self.adb_obj,
            ["shell", "getprop", "persist.test.prop"],
            timeout=5.0,
            attempts=adb._DEFAULT_RUN_ATTEMPTS,
        )

    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_setprop_quotes_special_characters(
        self, mock_run: mock.Mock
    ) -> None:
        """Test setprop quotes prop_name with spaces/metacharacters and empty value."""
        self.adb_obj.setprop("debug.foo bar; echo $HOME", "")

        mock_run.assert_called_once_with(
            self.adb_obj,
            ["shell", "setprop", "'debug.foo bar; echo $HOME'", "''"],
            timeout=None,
            attempts=adb._DEFAULT_RUN_ATTEMPTS,
        )

    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_getprop_quotes_special_characters(
        self, mock_run: mock.Mock
    ) -> None:
        """Test getprop quotes prop_name with spaces and shell metacharacters."""
        mock_run.return_value = ""

        self.adb_obj.getprop("debug.foo bar; echo $HOME")

        mock_run.assert_called_once_with(
            self.adb_obj,
            ["shell", "getprop", "'debug.foo bar; echo $HOME'"],
            timeout=None,
            attempts=adb._DEFAULT_RUN_ATTEMPTS,
        )

    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_forward_passes_no_rebind(self, mock_run: mock.Mock) -> None:
        """Test forward passes --no-rebind when creating a binding."""
        args = [f"tcp:{_HOST_PORT}", f"tcp:{_DEVICE_PORT}"]

        self.adb_obj.forward(args, timeout=5.0)

        mock_run.assert_called_once_with(
            self.adb_obj,
            ["forward", "--no-rebind", *args],
            timeout=5.0,
            attempts=adb._DEFAULT_RUN_ATTEMPTS,
        )

    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_forward_omits_no_rebind(self, mock_run: mock.Mock) -> None:
        """Test forward omits --no-rebind for option-only invocations."""
        for args in (
            ["--list"],
            ["--remove", f"tcp:{_HOST_PORT}"],
            ["--remove-all"],
        ):
            with self.subTest(args=args):
                mock_run.reset_mock()
                self.adb_obj.forward(args)
                mock_run.assert_called_once_with(
                    self.adb_obj,
                    ["forward", *args],
                    timeout=None,
                    attempts=adb._DEFAULT_RUN_ATTEMPTS,
                )

    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_reverse(self, mock_run: mock.Mock) -> None:
        """Test reverse runs `adb reverse` with the given args."""
        args = [f"tcp:{_DEVICE_PORT}", f"tcp:{_HOST_PORT}"]

        self.adb_obj.reverse(args, timeout=5.0)

        mock_run.assert_called_once_with(
            self.adb_obj,
            ["reverse", *args],
            timeout=5.0,
            attempts=adb._DEFAULT_RUN_ATTEMPTS,
        )

    def test_forward_and_reverse_hold_port_lock(self) -> None:
        """Test forward and reverse hold _ADB_PORT_LOCK while running."""
        lock_states: list[bool] = []

        def _record_lock_state(*args: Any, **kwargs: Any) -> str:
            lock_states.append(adb._ADB_PORT_LOCK.locked())
            return ""

        args = [f"tcp:{_HOST_PORT}", f"tcp:{_DEVICE_PORT}"]
        with mock.patch.object(
            adb.Adb, "run", autospec=True, side_effect=_record_lock_state
        ):
            self.adb_obj.forward(args)
            self.adb_obj.reverse(args)

        self.assertEqual(lock_states, [True, True])
        self.assertFalse(adb._ADB_PORT_LOCK.locked())

    @mock.patch.object(
        host_shell,
        "run",
        side_effect=errors.HoneydewTimeoutError("timeout"),
        autospec=True,
    )
    def test_custom_default_attempts_in_init(
        self, mock_host_run: mock.Mock
    ) -> None:
        """Test Adb(..., attempts=1) defaults commands to 1 attempt without retries."""
        with mock.patch.object(adb.Adb, "_cache_adbd_pid"):
            obj = adb.Adb(
                device_name=_DEVICE_NAME,
                serial_number=_SERIAL_NUMBER,
                ffx_transport=self.mock_ffx,
                run_isolated_server=False,
                attempts=1,
            )
        try:
            self.assertEqual(obj._attempts, 1)
            with self.assertRaises(adb_errors.AdbTimeoutError):
                obj.run(["shell", "echo", "hi"], timeout=10.0)
            mock_host_run.assert_called_once()
        finally:
            obj.close()


class ResolveVendorKeysPathTests(unittest.TestCase):
    """Unit tests for _resolve_vendor_keys_path."""

    def test_explicit_path_non_tar(self) -> None:
        """Test explicit path that is not a tar file is returned as-is."""
        with mock.patch("os.path.exists", return_value=True):
            path, temp_dir = adb._resolve_vendor_keys_path("/custom/keys/dir")
            self.assertEqual(path, "/custom/keys/dir")
            self.assertIsNone(temp_dir)

    def test_env_var_fallback(self) -> None:
        """Test fallback to ADB_VENDOR_KEYS environment variable."""
        with (
            mock.patch("os.path.exists", return_value=True),
            mock.patch.dict(os.environ, {"ADB_VENDOR_KEYS": "/env/keys"}),
        ):
            path, temp_dir = adb._resolve_vendor_keys_path(None)
            self.assertEqual(path, "/env/keys")
            self.assertIsNone(temp_dir)

    def test_nonexistent_path_falls_back_to_bundled(self) -> None:
        """Test nonexistent vendor_keys_path logs warning and falls back to bundled keys."""
        mock_temp_dir = mock.MagicMock(spec=tempfile.TemporaryDirectory)
        mock_temp_dir.name = "/bundled/extracted/keys"
        with (
            mock.patch.dict(os.environ, {}, clear=True),
            mock.patch.object(
                adb, "_get_bundled_keys_tar", return_value=mock_temp_dir
            ) as mock_bundled,
        ):
            path, temp_dir = adb._resolve_vendor_keys_path(
                "/nonexistent/keys.tar"
            )
            mock_bundled.assert_called_once()
            self.assertEqual(path, "/bundled/extracted/keys")
            self.assertEqual(temp_dir, mock_temp_dir)

    def test_tar_extraction_does_not_mutate_env_var(self) -> None:
        """Test extracting a valid tar archive without modifying os.environ."""
        with tempfile.TemporaryDirectory() as test_dir:
            tar_path = os.path.join(test_dir, "keys.tar")
            with tarfile.open(tar_path, "w") as tar:
                key_content = b"test_key_content"
                info = tarfile.TarInfo(name="test.adb_key")
                info.size = len(key_content)
                tar.addfile(info, io.BytesIO(key_content))

            with mock.patch.dict(os.environ, {}, clear=True):
                path, temp_dir = adb._resolve_vendor_keys_path(tar_path)
                try:
                    self.assertIsNotNone(temp_dir)
                    assert temp_dir is not None
                    self.assertEqual(path, temp_dir.name)
                    self.assertNotIn("ADB_VENDOR_KEYS", os.environ)
                    extracted_file = os.path.join(temp_dir.name, "test.adb_key")
                    self.assertTrue(os.path.exists(extracted_file))
                    with open(extracted_file, "rb") as f:
                        self.assertEqual(f.read(), key_content)
                finally:
                    if temp_dir:
                        temp_dir.cleanup()

    def test_tar_with_symlink_raises(self) -> None:
        """Test that tar with symlink raises AdbError."""
        with tempfile.TemporaryDirectory() as test_dir:
            tar_path = os.path.join(test_dir, "symlink_keys.tar")
            with tarfile.open(tar_path, "w") as tar:
                info = tarfile.TarInfo(name="symlink_key")
                info.type = tarfile.SYMTYPE
                info.linkname = "/etc/passwd"
                tar.addfile(info)

            with self.assertRaises(adb_errors.AdbError):
                adb._resolve_vendor_keys_path(tar_path)

    def test_tar_with_traversal_raises(self) -> None:
        """Test that tar with path traversal raises AdbError."""
        with tempfile.TemporaryDirectory() as test_dir:
            tar_path = os.path.join(test_dir, "traversal_keys.tar")
            with tarfile.open(tar_path, "w") as tar:
                info = tarfile.TarInfo(name="../escape.key")
                info.size = 0
                tar.addfile(info, io.BytesIO(b""))

            with self.assertRaises(adb_errors.AdbError):
                adb._resolve_vendor_keys_path(tar_path)

    def test_python_data_resources_fallback(self) -> None:
        """Test fallback to Python data resources (honeydew.data/adb_keys.tar)."""
        with tempfile.TemporaryDirectory() as test_dir:
            tar_path = os.path.join(test_dir, "adb_keys.tar")
            with tarfile.open(tar_path, "w") as tar:
                key_content = b"bundled_key_content"
                info = tarfile.TarInfo(name="bundled.adb_key")
                info.size = len(key_content)
                tar.addfile(info, io.BytesIO(key_content))

            mock_resource = mock.MagicMock()
            mock_resource.joinpath.return_value = Path(tar_path)

            with (
                mock.patch.dict(sys.modules, {"honeydew.data": mock.Mock()}),
                mock.patch.dict(os.environ, {}, clear=True),
                mock.patch.object(
                    resources, "files", return_value=mock_resource
                ),
            ):
                path, temp_dir = adb._resolve_vendor_keys_path(None)
                try:
                    self.assertIsNotNone(temp_dir)
                    assert temp_dir is not None
                    self.assertEqual(path, temp_dir.name)
                    self.assertNotIn("ADB_VENDOR_KEYS", os.environ)
                    extracted_file = os.path.join(
                        temp_dir.name, "bundled.adb_key"
                    )
                    self.assertTrue(os.path.exists(extracted_file))
                    with open(extracted_file, "rb") as f:
                        self.assertEqual(f.read(), key_content)
                finally:
                    if temp_dir:
                        temp_dir.cleanup()

    def test_real_bundled_keys_tar_end_to_end(self) -> None:
        """Test end-to-end extraction of the actual bundled honeydew.data/adb_keys.tar."""
        has_bundled_tar = False
        try:
            from honeydew import data  # type: ignore[attr-defined]

            has_bundled_tar = (
                resources.files(data).joinpath("adb_keys.tar").is_file()
            )
        except (ImportError, FileNotFoundError, AttributeError, TypeError):
            pass

        with mock.patch.dict(os.environ, {}, clear=True):
            path, temp_dir = adb._resolve_vendor_keys_path(None)
            try:
                if has_bundled_tar:
                    self.assertIsNotNone(temp_dir)
                    assert temp_dir is not None
                    self.assertEqual(path, temp_dir.name)
                    self.assertNotIn("ADB_VENDOR_KEYS", os.environ)
                    extracted_keys = [
                        f
                        for f in os.listdir(temp_dir.name)
                        if f.endswith(".adb_key")
                    ]
                    self.assertGreater(len(extracted_keys), 0)
                else:
                    self.assertIsNone(path)
                    self.assertIsNone(temp_dir)
            finally:
                if temp_dir:
                    temp_dir.cleanup()


class AdbServerTests(unittest.TestCase):
    """Unit tests for AdbServer."""

    def test_check_for_conflicting_adb_servers_kills_conflicting_pids(
        self,
    ) -> None:
        """Test _check_for_conflicting_adb_servers kills conflicting server PIDs."""
        server = adb_server.AdbServer(
            adb_binary_path="/custom/adb",
            serial_id="target_serial",
        )
        server._process = mock.Mock(pid=1000)
        pgrep_output = "\n".join(
            [
                f"{os.getpid()} adb -P 5037 fork-server server",
                "1000 adb -P 5037 fork-server server",
                "1001 adb -P 5037 fork-server server",
                "1002 adb -P 60855 --one-device target_serial server nodaemon",
                "1003 adb -P 60856 --one-device other_serial server nodaemon",
                "1004 adb -s target_serial wait-for-device",
            ]
        )
        with (
            mock.patch.object(
                subprocess,
                "run",
                return_value=subprocess.CompletedProcess(
                    args=["pgrep", "-u", str(os.getuid()), "-af", "adb"],
                    returncode=0,
                    stdout=pgrep_output,
                    stderr="",
                ),
            ),
            mock.patch.object(server, "_read_proc_cmdline", return_value=[]),
            mock.patch.object(server, "_is_zombie_process", return_value=False),
            mock.patch.object(
                os,
                "kill",
                side_effect=[
                    None,  # SIGKILL 1001
                    None,  # SIGKILL 1002
                    None,  # Poll 1: 1001 still alive
                    ProcessLookupError(),  # Poll 1: 1002 exited
                    ProcessLookupError(),  # Poll 2: 1001 exited
                ],
            ) as mock_kill,
            mock.patch.object(time, "sleep") as mock_sleep,
        ):
            server._check_for_conflicting_adb_servers()
            self.assertEqual(
                mock_kill.call_args_list,
                [
                    mock.call(1001, signal.SIGKILL),
                    mock.call(1002, signal.SIGKILL),
                    mock.call(1001, 0),
                    mock.call(1002, 0),
                    mock.call(1001, 0),
                ],
            )
            mock_sleep.assert_called_once_with(0.05)

    def test_check_for_conflicting_adb_servers_ignores_pgrep_failure(
        self,
    ) -> None:
        """Test _check_for_conflicting_adb_servers skips the check when pgrep
        cannot be run or its output cannot be decoded."""
        server = adb_server.AdbServer(
            adb_binary_path="/custom/adb",
            serial_id="target_serial",
        )
        for err in [
            FileNotFoundError("pgrep"),
            UnicodeDecodeError("utf-8", b"\xff", 0, 1, "invalid start byte"),
        ]:
            with (
                self.subTest(err=err),
                mock.patch.object(
                    subprocess,
                    "run",
                    side_effect=err,
                ) as mock_run,
                mock.patch.object(os, "kill") as mock_kill,
            ):
                server._check_for_conflicting_adb_servers()

                mock_run.assert_called_once()
                mock_kill.assert_not_called()

    def test_check_for_conflicting_adb_servers_propagates_unexpected_exceptions(
        self,
    ) -> None:
        """Test _check_for_conflicting_adb_servers propagates unexpected
        exceptions (e.g. Mobly signals) raised while running pgrep."""
        server = adb_server.AdbServer(
            adb_binary_path="/custom/adb",
            serial_id="target_serial",
        )
        with (
            mock.patch.object(
                subprocess,
                "run",
                side_effect=signals.TestAbortAll("abort"),
            ) as mock_run,
            mock.patch.object(os, "kill") as mock_kill,
        ):
            with self.assertRaises(signals.TestAbortAll):
                server._check_for_conflicting_adb_servers()

            mock_run.assert_called_once()
            mock_kill.assert_not_called()

    def test_set_pdeathsig_on_linux(self) -> None:
        """Test _set_pdeathsig sets PR_SET_PDEATHSIG to SIGKILL on Linux."""
        mock_libc = mock.Mock()
        with mock.patch.object(adb_server, "_LIBC", mock_libc):
            adb_server._set_pdeathsig()
            mock_libc.prctl.assert_called_once_with(
                adb_server._PR_SET_PDEATHSIG, signal.SIGKILL
            )


if __name__ == "__main__":
    unittest.main()
