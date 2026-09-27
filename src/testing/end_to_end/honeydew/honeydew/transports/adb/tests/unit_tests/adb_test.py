# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for adb.py."""

import asyncio
import io
import os
import sys
import tarfile
import tempfile
import unittest
from importlib import resources
from pathlib import Path
from typing import Any
from unittest import mock

from honeydew import errors
from honeydew.transports.adb import adb
from honeydew.transports.adb import errors as adb_errors
from honeydew.utils import host_shell

_DEVICE_NAME = "fuchsia-mock-device"
_SERIAL_NUMBER = "12345678"


# pylint: disable=protected-access
class AdbTests(unittest.TestCase):
    """Unit tests for ADB transport."""

    def setUp(self) -> None:
        super().setUp()

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
        with mock.patch.object(adb.Adb, "_cache_adbd_pid"):
            self.adb_obj = adb.Adb(
                device_name=_DEVICE_NAME,
                serial_number=_SERIAL_NUMBER,
                ffx_transport=self.mock_ffx,
                run_isolated_server=False,
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
        with mock.patch.dict(os.environ, {}, clear=True):
            obj = adb.Adb(
                device_name=_DEVICE_NAME,
                serial_number=_SERIAL_NUMBER,
                ffx_transport=self.mock_ffx,
                run_isolated_server=False,
            )
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
        """Test init starts isolated server by default (run_isolated_server=True)."""
        mock_server = mock.Mock()
        mock_adb_server_cls.return_value = mock_server

        obj = adb.Adb(
            device_name=_DEVICE_NAME,
            serial_number=_SERIAL_NUMBER,
            ffx_transport=self.mock_ffx,
            vendor_keys_path="/keys",
        )
        mock_adb_server_cls.assert_called_once_with(
            adb_binary_path="/custom/adb",
            serial_id=_SERIAL_NUMBER,
            vendor_keys_path="/keys",
        )
        mock_server.start.assert_called_once()
        mock_cache_pid.assert_called_once_with(obj)
        self.assertIs(obj._adb_server, mock_server)
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
        mock_check.assert_called_once()
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
            mock_check.assert_called_once_with()
            mock_cache.assert_called_once_with()

        self.assertFalse(self.adb_obj.is_root)
        self.assertFalse(self.adb_obj._rooted_by_context)
        self.assertEqual(self.adb_obj._root_ref_count, 0)

    @mock.patch("time.time", side_effect=[100.0, 103.0], autospec=True)
    @mock.patch.object(adb.Adb, "wait_for_boot_complete", autospec=True)
    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_check_connection_success(
        self,
        mock_run: mock.Mock,
        mock_wait_for_boot: mock.Mock,
        mock_time: mock.Mock,
    ) -> None:
        """Test check_connection deducts elapsed time before calling wait_for_boot_complete."""
        self._check_connection_patcher.stop()
        try:
            self.adb_obj.check_connection(timeout=10.0)
            mock_run.assert_called_once_with(
                self.adb_obj, ["wait-for-device"], timeout=10.0
            )
            mock_wait_for_boot.assert_called_once_with(
                self.adb_obj, timeout=7.0
            )
        finally:
            self._check_connection_patcher.start()

    @mock.patch("time.time", side_effect=[100.0, 100.0], autospec=True)
    @mock.patch.object(adb.Adb, "wait_for_boot_complete", autospec=True)
    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_check_connection_default_timeout(
        self,
        mock_run: mock.Mock,
        mock_wait_for_boot: mock.Mock,
        mock_time: mock.Mock,
    ) -> None:
        """Test check_connection uses default timeout of 300.0s when not specified."""
        self._check_connection_patcher.stop()
        try:
            self.adb_obj.check_connection()
            mock_run.assert_called_once_with(
                self.adb_obj, ["wait-for-device"], timeout=300.0
            )
            mock_wait_for_boot.assert_called_once_with(
                self.adb_obj, timeout=300.0
            )
        finally:
            self._check_connection_patcher.start()

    @mock.patch("time.time", side_effect=[100.0, 110.0], autospec=True)
    @mock.patch.object(adb.Adb, "wait_for_boot_complete", autospec=True)
    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_check_connection_timeout_expired(
        self,
        mock_run: mock.Mock,
        mock_wait_for_boot: mock.Mock,
        mock_time: mock.Mock,
    ) -> None:
        """Test check_connection raises AdbConnectionError if timeout expires during wait-for-device."""
        self._check_connection_patcher.stop()
        try:
            with self.assertRaises(adb_errors.AdbConnectionError):
                self.adb_obj.check_connection(timeout=10.0)
            mock_wait_for_boot.assert_not_called()
        finally:
            self._check_connection_patcher.start()

    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_check_connection_failure_wait_for_device(
        self, mock_run: mock.Mock
    ) -> None:
        """Test check_connection raises AdbConnectionError when wait-for-device fails."""
        self._check_connection_patcher.stop()
        try:
            mock_run.side_effect = adb_errors.AdbCommandError(
                "wait-for-device failed"
            )
            with self.assertRaises(adb_errors.AdbConnectionError):
                self.adb_obj.check_connection()
        finally:
            self._check_connection_patcher.start()

    @mock.patch.object(adb.Adb, "wait_for_boot_complete", autospec=True)
    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_check_connection_failure_wait_for_boot_complete(
        self, mock_run: mock.Mock, mock_wait_for_boot: mock.Mock
    ) -> None:
        """Test check_connection raises AdbConnectionError when wait_for_boot_complete fails."""
        self._check_connection_patcher.stop()
        try:
            mock_wait_for_boot.side_effect = adb_errors.AdbTimeoutError(
                "boot timeout"
            )
            with self.assertRaises(adb_errors.AdbConnectionError):
                self.adb_obj.check_connection()
        finally:
            self._check_connection_patcher.start()

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
                self.adb_obj.check_connection()
        finally:
            self._check_connection_patcher.start()

    @mock.patch.object(
        host_shell,
        "run",
        side_effect=errors.HoneydewTimeoutError("timed out"),
        autospec=True,
    )
    def test_run_timeout(self, mock_host_shell_run: mock.Mock) -> None:
        """Test run timeout path."""
        with self.assertRaises(adb_errors.AdbTimeoutError):
            self.adb_obj.run(["shell", "long_running"], timeout=0.1)

    @mock.patch.object(host_shell, "run", autospec=True)
    @mock.patch("time.sleep", autospec=True)
    def test_run_retry_success(
        self, mock_sleep: mock.Mock, mock_host_shell_run: mock.Mock
    ) -> None:
        """Test run retries on connection error and succeeds."""
        # Set a mocked ADB server
        mock_server = mock.Mock()
        mock_server.host.return_value = "127.0.0.1"
        mock_server.port.return_value = 12345
        self.adb_obj._adb_server = mock_server

        # 1. First attempt: fail with "device not found"
        # 2. _recover_adb_server -> _cache_adbd_pid() runs "shell pidof adbd"
        # 3. Second attempt: succeed
        mock_host_shell_run.side_effect = [
            errors.HostCmdError("error: device not found"),
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
        self.assertTrue(self.adb_obj.is_root)
        self.assertEqual(self.adb_obj._cached_adbd_pid, "8765")

    def test_close(self) -> None:
        """Test close stops the adb server if running."""
        mock_server = mock.Mock()
        self.adb_obj._adb_server = mock_server
        self.adb_obj.close()
        mock_server.stop.assert_called_once()
        self.assertIs(self.adb_obj._adb_server, mock_server)

    def test_close_cleans_up_vendor_keys(self) -> None:
        """Test close cleans up temp vendor keys dir if present."""
        mock_temp_dir = mock.Mock()
        self.adb_obj._temp_vendor_keys_dir = mock_temp_dir
        self.adb_obj.close()
        mock_temp_dir.cleanup.assert_called_once()
        self.assertIsNone(self.adb_obj._temp_vendor_keys_dir)

    def test_is_root_default(self) -> None:
        """Test is_root returns False by default."""
        self.assertFalse(self.adb_obj.is_root)

    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_root(self, mock_run: mock.Mock) -> None:
        """Test root runs root and wait-for-device commands and updates is_root."""
        self.adb_obj.root(timeout=10.0)

        mock_run.assert_has_calls(
            [
                mock.call(self.adb_obj, ["root"], timeout=10.0),
                mock.call(self.adb_obj, ["wait-for-device"], timeout=10.0),
            ]
        )
        self.assertTrue(self.adb_obj.is_root)

    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_unroot(self, mock_run: mock.Mock) -> None:
        """Test unroot runs unroot and wait-for-device commands and updates is_root."""
        self.adb_obj._is_root = True

        self.adb_obj.unroot(timeout=10.0)

        mock_run.assert_has_calls(
            [
                mock.call(self.adb_obj, ["unroot"], timeout=10.0),
                mock.call(self.adb_obj, ["wait-for-device"], timeout=10.0),
            ]
        )
        self.assertFalse(self.adb_obj.is_root)

    @mock.patch.object(adb.Adb, "unroot", autospec=True)
    @mock.patch.object(adb.Adb, "root", autospec=True)
    def test_use_adb_root_when_not_root(
        self, mock_root: mock.Mock, mock_unroot: mock.Mock
    ) -> None:
        """Test use_adb_root roots on enter and unroots on exit when not root initially."""
        self.assertFalse(self.adb_obj.is_root)

        with self.adb_obj.use_adb_root(timeout=5.0):
            mock_root.assert_called_once_with(self.adb_obj, timeout=5.0)
            mock_unroot.assert_not_called()

        mock_unroot.assert_called_once_with(self.adb_obj, timeout=5.0)

    @mock.patch.object(adb.Adb, "unroot", autospec=True)
    @mock.patch.object(adb.Adb, "root", autospec=True)
    def test_use_adb_root_when_already_root(
        self, mock_root: mock.Mock, mock_unroot: mock.Mock
    ) -> None:
        """Test use_adb_root does not root or unroot when already root."""
        self.adb_obj._is_root = True

        with self.adb_obj.use_adb_root(timeout=5.0):
            mock_root.assert_not_called()
            mock_unroot.assert_not_called()

        mock_root.assert_not_called()
        mock_unroot.assert_not_called()

    @mock.patch.object(adb.Adb, "unroot", autospec=True)
    @mock.patch.object(adb.Adb, "root", autospec=True)
    def test_use_adb_root_async_context(
        self, mock_root: mock.Mock, mock_unroot: mock.Mock
    ) -> None:
        """Test use_adb_root works as an async context manager."""
        self.assertFalse(self.adb_obj.is_root)

        async def run_async() -> None:
            async with self.adb_obj.use_adb_root(timeout=5.0):
                mock_root.assert_called_once_with(self.adb_obj, timeout=5.0)
                mock_unroot.assert_not_called()

        asyncio.run(run_async())
        mock_unroot.assert_called_once_with(self.adb_obj, timeout=5.0)

    @mock.patch.object(adb.Adb, "unroot", autospec=True)
    @mock.patch.object(adb.Adb, "root", autospec=True)
    def test_use_adb_root_concurrent_overlapping(
        self, mock_root: mock.Mock, mock_unroot: mock.Mock
    ) -> None:
        """Test overlapping use_adb_root contexts only unroot when last task exits."""
        self.assertFalse(self.adb_obj.is_root)

        def side_effect_root(
            inst: adb.Adb, timeout: float | None = None
        ) -> None:
            inst._is_root = True

        def side_effect_unroot(
            inst: adb.Adb, timeout: float | None = None
        ) -> None:
            inst._is_root = False

        mock_root.side_effect = side_effect_root
        mock_unroot.side_effect = side_effect_unroot

        ctx_a = self.adb_obj.use_adb_root(timeout=5.0)
        ctx_b = self.adb_obj.use_adb_root(timeout=5.0)

        # Task A enters -> roots device (depth 1)
        ctx_a.__enter__()
        mock_root.assert_called_once_with(self.adb_obj, timeout=5.0)
        mock_unroot.assert_not_called()

        # Task B enters while A is active -> does not re-root (depth 2)
        ctx_b.__enter__()
        mock_root.assert_called_once()
        mock_unroot.assert_not_called()

        # Task A exits first -> must NOT unroot because Task B is still active (depth 1)
        ctx_a.__exit__(None, None, None)
        mock_unroot.assert_not_called()
        self.assertTrue(self.adb_obj.is_root)

        # Task B exits -> unroots device as depth returns to 0
        ctx_b.__exit__(None, None, None)
        mock_unroot.assert_called_once_with(self.adb_obj, timeout=5.0)
        self.assertFalse(self.adb_obj.is_root)

    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_setprop(self, mock_run: mock.Mock) -> None:
        """Test setprop runs adb shell setprop command."""
        self.adb_obj.setprop("persist.test.prop", "val123", timeout=5.0)

        mock_run.assert_called_once_with(
            self.adb_obj,
            ["shell", "setprop", "persist.test.prop", "val123"],
            timeout=5.0,
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
        )

    @mock.patch.object(adb.Adb, "getprop", autospec=True)
    def test_wait_for_boot_complete_success_immediately(
        self, mock_getprop: mock.Mock
    ) -> None:
        """Test wait_for_boot_complete succeeds immediately when sys.boot_completed is 1."""
        mock_getprop.return_value = "1"

        self.adb_obj.wait_for_boot_complete()

        mock_getprop.assert_called_once_with(self.adb_obj, "sys.boot_completed")

    @mock.patch("time.sleep", autospec=True)
    @mock.patch.object(adb.Adb, "getprop", autospec=True)
    def test_wait_for_boot_complete_success_after_polling(
        self, mock_getprop: mock.Mock, mock_sleep: mock.Mock
    ) -> None:
        """Test wait_for_boot_complete polls until sys.boot_completed is 1."""
        mock_getprop.side_effect = ["0", "", "1"]

        self.adb_obj.wait_for_boot_complete(poll_interval=0.5)

        self.assertEqual(mock_getprop.call_count, 3)
        mock_sleep.assert_has_calls([mock.call(0.5), mock.call(0.5)])

    @mock.patch("time.sleep", autospec=True)
    @mock.patch.object(adb.Adb, "getprop", autospec=True)
    def test_wait_for_boot_complete_success_after_adb_errors(
        self, mock_getprop: mock.Mock, mock_sleep: mock.Mock
    ) -> None:
        """Test wait_for_boot_complete ignores AdbErrors during polling and succeeds."""
        mock_getprop.side_effect = [
            adb_errors.AdbCommandError("device offline"),
            "0",
            "1",
        ]

        self.adb_obj.wait_for_boot_complete(poll_interval=0.5)

        self.assertEqual(mock_getprop.call_count, 3)
        mock_sleep.assert_has_calls([mock.call(0.5), mock.call(0.5)])

    @mock.patch("time.sleep", autospec=True)
    @mock.patch("time.time", autospec=True)
    @mock.patch.object(adb.Adb, "getprop", autospec=True)
    def test_wait_for_boot_complete_timeout(
        self,
        mock_getprop: mock.Mock,
        mock_time: mock.Mock,
        mock_sleep: mock.Mock,
    ) -> None:
        """Test wait_for_boot_complete raises AdbTimeoutError when timeout expires."""
        mock_time.side_effect = [0.0, 1.0, 10.0]
        mock_getprop.return_value = "0"

        with self.assertRaises(adb_errors.AdbTimeoutError):
            self.adb_obj.wait_for_boot_complete(timeout=5.0)

        mock_getprop.assert_called_once_with(self.adb_obj, "sys.boot_completed")
        mock_sleep.assert_called_once_with(1.0)


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


if __name__ == "__main__":
    unittest.main()
