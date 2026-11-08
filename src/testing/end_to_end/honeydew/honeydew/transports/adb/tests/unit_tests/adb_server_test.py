# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for adb_server.py."""

import errno
import os
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path
from unittest import mock

from honeydew.transports.adb import adb_server
from honeydew.transports.adb import errors as adb_errors

_ADB_BINARY = "/custom/adb"
_SERIAL_ID = "12345678"
_OTHER_SERIAL_ID = "87654321"
_EPHEMERAL_PORT = 54321
_VENDOR_KEYS_PATH = "/path/to/vendor_keys"


# pylint: disable=protected-access
class AdbServerTests(unittest.TestCase):
    """Unit tests for AdbServer lifecycle, port allocation, and conflict detection."""

    def setUp(self) -> None:
        super().setUp()
        self._atexit_patcher = mock.patch("atexit.register", autospec=True)
        self.mock_atexit_register = self._atexit_patcher.start()

        self._real_read_proc_cmdline = adb_server.AdbServer._read_proc_cmdline
        self._proc_cmdline_patcher = mock.patch.object(
            adb_server.AdbServer,
            "_read_proc_cmdline",
            autospec=True,
            return_value=[],
        )
        self.mock_read_proc_cmdline = self._proc_cmdline_patcher.start()

        self._real_get_process_attribution = (
            adb_server.AdbServer._get_process_attribution
        )
        self._proc_attr_patcher = mock.patch.object(
            adb_server.AdbServer,
            "_get_process_attribution",
            autospec=True,
            return_value="",
        )
        self.mock_get_process_attribution = self._proc_attr_patcher.start()

        self._temp_dir = tempfile.TemporaryDirectory()
        self.output_dir = Path(self._temp_dir.name)

        self.server = adb_server.AdbServer(
            adb_binary_path=_ADB_BINARY,
            serial_id=_SERIAL_ID,
            output_dir=self.output_dir,
        )

    def tearDown(self) -> None:
        self._temp_dir.cleanup()
        mock.patch.stopall()
        super().tearDown()

    def test_init_registers_atexit_and_sets_defaults(self) -> None:
        """Test __init__ registers self.stop with atexit and initializes state."""
        # self.server is instantiated in setUp() while atexit.register is patched
        self.mock_atexit_register.assert_called_once_with(self.server.stop)
        self.assertEqual(self.server._adb_binary_path, _ADB_BINARY)
        self.assertEqual(self.server._serial_id, _SERIAL_ID)
        self.assertEqual(self.server._output_dir, self.output_dir)
        self.assertIsNone(self.server._vendor_keys_path)
        self.assertIsNone(self.server._process)
        self.assertIsNone(self.server._server_port)
        self.assertIsNone(self.server._server_thread)
        self.assertEqual(self.server._attempt_count, 0)
        self.assertEqual(self.server._server_id, 0)
        self.assertFalse(self.server._server_is_ready.is_set())
        self.assertFalse(self.server._server_stop_requested.is_set())

    def test_init_creates_output_dir_if_not_exists(self) -> None:
        """Test __init__ creates output directory if it does not exist."""
        nonexistent_dir = self.output_dir / "nested" / "logs" / "adb"
        self.assertFalse(nonexistent_dir.exists())

        server = adb_server.AdbServer(
            adb_binary_path=_ADB_BINARY,
            serial_id=_SERIAL_ID,
            output_dir=nonexistent_dir,
        )
        self.assertTrue(nonexistent_dir.exists())
        self.assertEqual(server._output_dir, nonexistent_dir)

    def test_init_output_dir_as_str_and_empty_str(self) -> None:
        """Test __init__ normalizes string output_dir and falls back on empty."""
        str_dir = str(self.output_dir / "str_dir")
        server = adb_server.AdbServer(
            adb_binary_path=_ADB_BINARY,
            serial_id=_SERIAL_ID,
            output_dir=str_dir,
        )
        self.assertEqual(server._output_dir, Path(str_dir))
        self.assertTrue(Path(str_dir).exists())

        empty_server = adb_server.AdbServer(
            adb_binary_path=_ADB_BINARY,
            serial_id=_SERIAL_ID,
            output_dir="",
        )
        self.assertEqual(empty_server._output_dir, Path(tempfile.gettempdir()))

    def test_host_and_port(self) -> None:
        """Test host() and port() accessors."""
        self.assertEqual(self.server.host(), "127.0.0.1")
        # port() raises AdbServerError if called before _server_port is assigned
        with self.assertRaisesRegex(
            adb_errors.AdbServerError,
            "ADB server is not running; no port is assigned.",
        ):
            self.server.port()

        self.server._server_port = _EPHEMERAL_PORT
        self.assertEqual(self.server.port(), _EPHEMERAL_PORT)

    @mock.patch("threading.Thread", autospec=True)
    def test_start_creates_daemon_thread_and_waits_for_ready(
        self, mock_thread_cls: mock.Mock
    ) -> None:
        """Test start() spawns a daemon thread and returns once ready flag is set."""
        mock_thread = mock.Mock()
        mock_thread.is_alive.return_value = True
        # Unblock start()'s readiness loop as soon as the thread starts
        mock_thread.start.side_effect = self.server._server_is_ready.set
        mock_thread_cls.return_value = mock_thread

        self.server.start()

        mock_thread_cls.assert_called_once_with(
            target=self.server._run_server,
            name=f"adb_server_thread_{_SERIAL_ID}",
            daemon=True,
        )
        mock_thread.start.assert_called_once_with()
        self.assertIs(self.server._server_thread, mock_thread)

    @mock.patch("time.sleep", autospec=True)
    @mock.patch("threading.Thread", autospec=True)
    def test_start_polls_until_ready(
        self, mock_thread_cls: mock.Mock, mock_sleep: mock.Mock
    ) -> None:
        """Test start() sleeps while waiting for ready flag to be set."""
        mock_thread = mock.Mock()
        mock_thread.is_alive.return_value = True
        mock_thread_cls.return_value = mock_thread

        # 1st False: initial check before spawning thread; 2nd False: 1st poll; 3rd True: 2nd poll
        with mock.patch.object(
            self.server._server_is_ready,
            "is_set",
            side_effect=[False, False, True],
        ):
            self.server.start()

        mock_sleep.assert_called_once_with(0.1)

    @mock.patch("threading.Thread", autospec=True)
    def test_start_server_thread_dies_before_ready_raises_error(
        self, mock_thread_cls: mock.Mock
    ) -> None:
        """Test start() raises AdbServerError if server thread dies before setting ready flag."""
        mock_thread = mock.Mock()
        mock_thread.is_alive.return_value = False
        mock_thread_cls.return_value = mock_thread

        with self.assertRaisesRegex(
            adb_errors.AdbServerError,
            "ADB server thread died before setting ready flag",
        ):
            self.server.start()

    @mock.patch("threading.Thread", autospec=True)
    def test_start_when_thread_already_alive_is_noop(
        self, mock_thread_cls: mock.Mock
    ) -> None:
        """Test start() does not spawn a new thread if one is already alive."""
        existing_thread = mock.Mock()
        existing_thread.is_alive.return_value = True
        self.server._server_thread = existing_thread

        self.server.start()

        mock_thread_cls.assert_not_called()

    @mock.patch("threading.Thread", autospec=True)
    def test_start_when_already_ready_is_noop(
        self, mock_thread_cls: mock.Mock
    ) -> None:
        """Test start() returns early if _server_is_ready is already set."""
        self.server._server_is_ready.set()

        self.server.start()

        mock_thread_cls.assert_not_called()

    @mock.patch("threading.Thread", autospec=True)
    def test_start_replaces_dead_thread_and_clears_stop_requested(
        self, mock_thread_cls: mock.Mock
    ) -> None:
        """Test start() cleans up a dead thread reference before starting a new one."""
        # Simulate a leftover dead thread and stale stop flag from a previous run
        dead_thread = mock.Mock()
        dead_thread.is_alive.return_value = False
        self.server._server_thread = dead_thread
        self.server._server_stop_requested.set()

        new_thread = mock.Mock()
        new_thread.is_alive.return_value = True
        new_thread.start.side_effect = self.server._server_is_ready.set
        mock_thread_cls.return_value = new_thread

        self.server.start()

        self.assertFalse(self.server._server_stop_requested.is_set())
        self.assertIs(self.server._server_thread, new_thread)

    def test_start_exceeds_max_restarts_raises_error(self) -> None:
        """Test start() raises AdbServerError when restart count reaches limit."""
        self.server._attempt_count = adb_server.AdbServer._MAX_SERVER_RESTARTS

        with self.assertRaisesRegex(
            adb_errors.AdbServerError,
            "Exceeded maximum number of ADB server restarts",
        ):
            self.server.start()

    def test_stop_when_already_stopped_is_noop(self) -> None:
        """Test stop() returns early when _server_thread is None."""
        self.server._server_thread = None
        self.server.stop()
        self.assertFalse(self.server._server_stop_requested.is_set())

    def test_stop_graceful_termination_sigterm(self) -> None:
        """Test stop() terminates the process gracefully with SIGTERM when it exits in time."""
        mock_proc = mock.Mock()
        mock_thread = mock.Mock()
        # Simulate thread exiting cleanly by the time join(timeout=5) finishes
        mock_thread.is_alive.return_value = False
        self.server._process = mock_proc
        self.server._server_thread = mock_thread

        self.server.stop()

        mock_proc.terminate.assert_called_once_with()
        mock_proc.wait.assert_called_once_with(timeout=3)
        mock_proc.kill.assert_not_called()
        mock_thread.join.assert_called_once_with(timeout=5)
        self.assertIsNone(self.server._server_thread)
        self.assertFalse(self.server._server_stop_requested.is_set())

    def test_stop_forceful_termination_sigkill_on_timeout(self) -> None:
        """Test stop() escalates from SIGTERM to SIGKILL if process.wait times out."""
        mock_proc = mock.Mock()
        # 1st wait(timeout=3) after SIGTERM times out; 2nd wait() after SIGKILL succeeds
        mock_proc.wait.side_effect = [
            subprocess.TimeoutExpired(cmd="adb", timeout=3),
            0,
        ]
        mock_thread = mock.Mock()
        mock_thread.is_alive.return_value = False
        self.server._process = mock_proc
        self.server._server_thread = mock_thread

        self.server.stop()

        mock_proc.terminate.assert_called_once_with()
        mock_proc.kill.assert_called_once_with()
        self.assertEqual(
            mock_proc.wait.call_args_list,
            [mock.call(timeout=3), mock.call()],
        )
        mock_thread.join.assert_called_once_with(timeout=5)
        self.assertIsNone(self.server._server_thread)

    def test_stop_keeps_thread_reference_if_still_alive_after_join(
        self,
    ) -> None:
        """Test stop() does not clear _server_thread if thread fails to exit after join."""
        mock_thread = mock.Mock()
        # Simulate a hung thread that remains alive even after join(timeout=5)
        mock_thread.is_alive.return_value = True
        self.server._server_thread = mock_thread

        self.server.stop()

        mock_thread.join.assert_called_once_with(timeout=5)
        self.assertIs(self.server._server_thread, mock_thread)
        self.assertTrue(self.server._server_stop_requested.is_set())

    def test_stop_reraises_unexpected_exception(self) -> None:
        """Test stop() logs and re-raises unexpected errors during termination."""
        mock_proc = mock.Mock()
        mock_proc.terminate.side_effect = RuntimeError("terminate failed")
        self.server._process = mock_proc
        self.server._server_thread = mock.Mock()

        with self.assertRaises(RuntimeError):
            self.server.stop()

    def test_restart_calls_stop_and_start(self) -> None:
        """Test restart() stops and then starts the server."""
        with (
            mock.patch.object(self.server, "stop") as mock_stop,
            mock.patch.object(self.server, "start") as mock_start,
        ):
            self.server.restart()
            mock_stop.assert_called_once_with()
            mock_start.assert_called_once_with()

    def test_reset_restart_count(self) -> None:
        """Test reset_restart_count() resets _attempt_count to 0 while preserving _server_id."""
        self.server._attempt_count = 2
        self.server._server_id = 5
        self.server.reset_restart_count()
        self.assertEqual(self.server._attempt_count, 0)
        self.assertEqual(self.server._server_id, 5)

    @mock.patch("os.kill", autospec=True)
    @mock.patch("subprocess.run", autospec=True)
    def test_check_for_conflicting_adb_servers_without_one_device(
        self, mock_run: mock.Mock, mock_kill: mock.Mock
    ) -> None:
        """Test _check_for_conflicting_adb_servers logs error for servers without --one-device."""
        # Both "fork-server" and "server nodaemon" without --one-device conflict with all devices
        mock_run.return_value = subprocess.CompletedProcess(
            args=["pgrep", "-u", str(os.getuid()), "-af", "adb"],
            returncode=0,
            stdout=(
                "1001 /usr/bin/adb -L tcp:5037 fork-server server --reply-fd 4\n"
                "1002 /custom/adb -P 5037 server nodaemon\n"
            ),
            stderr="",
        )
        # First two calls send SIGKILL; next two calls (signal 0) raise ProcessLookupError once dead
        mock_kill.side_effect = [
            None,
            None,
            ProcessLookupError,
            ProcessLookupError,
        ]

        with self.assertLogs(adb_server._LOGGER, level="ERROR") as cm:
            self.server._check_for_conflicting_adb_servers()

        mock_run.assert_called_once_with(
            ["pgrep", "-u", str(os.getuid()), "-af", "adb"],
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(len(cm.output), 2)
        self.assertIn("[UNSCOPED]", cm.output[0])
        self.assertIn("PID: 1001", cm.output[0])
        self.assertIn("[UNSCOPED]", cm.output[1])
        self.assertIn("PID: 1002", cm.output[1])
        self.assertEqual(
            mock_kill.call_args_list,
            [
                mock.call(1001, signal.SIGKILL),
                mock.call(1002, signal.SIGKILL),
                mock.call(1001, 0),
                mock.call(1002, 0),
            ],
        )

    @mock.patch("time.sleep", autospec=True)
    @mock.patch("os.kill", autospec=True)
    @mock.patch("subprocess.run", autospec=True)
    def test_check_for_conflicting_adb_servers_matching_serial(
        self,
        mock_run: mock.Mock,
        mock_kill: mock.Mock,
        mock_sleep: mock.Mock,
    ) -> None:
        """Test _check_for_conflicting_adb_servers logs error for server with matching serial."""
        mock_run.return_value = subprocess.CompletedProcess(
            args=["pgrep", "-u", str(os.getuid()), "-af", "adb"],
            returncode=0,
            stdout=(
                f"2001 /tmp/tmp1234567890abcdefadb -P 51000 --one-device {_SERIAL_ID} server nodaemon\n"
            ),
            stderr="",
        )
        # 1st call sends SIGKILL; 2nd call (signal 0) succeeds while process is still alive;
        # 3rd call (signal 0) raises ProcessLookupError after sleeping 0.05s
        mock_kill.side_effect = [None, None, ProcessLookupError]

        with (
            mock.patch.object(
                adb_server.AdbServer,
                "_is_zombie_process",
                autospec=True,
                return_value=False,
            ),
            self.assertLogs(adb_server._LOGGER, level="ERROR") as cm,
        ):
            self.server._check_for_conflicting_adb_servers()

        self.assertEqual(len(cm.output), 1)
        self.assertIn("[ACTIVE DUT]", cm.output[0])
        self.assertIn("PID: 2001", cm.output[0])
        self.assertIn(_SERIAL_ID, cm.output[0])
        self.assertEqual(
            mock_kill.call_args_list,
            [
                mock.call(2001, signal.SIGKILL),
                mock.call(2001, 0),
                mock.call(2001, 0),
            ],
        )
        mock_sleep.assert_called_once_with(0.05)

    @mock.patch("os.kill", autospec=True)
    @mock.patch("subprocess.run", autospec=True)
    def test_check_for_conflicting_adb_servers_kill_oserror(
        self, mock_run: mock.Mock, mock_kill: mock.Mock
    ) -> None:
        """Test _check_for_conflicting_adb_servers logs warning if os.kill(SIGKILL) raises OSError."""
        mock_run.return_value = subprocess.CompletedProcess(
            args=["pgrep", "-a", "adb"],
            returncode=0,
            stdout=(
                f"2001 /custom/adb -P 51000 --one-device {_SERIAL_ID} server nodaemon\n"
            ),
            stderr="",
        )
        # Simulate process exiting between pgrep and os.kill(pid, SIGKILL)
        mock_kill.side_effect = ProcessLookupError("No such process")

        with self.assertLogs(adb_server._LOGGER, level="WARNING") as cm:
            self.server._check_for_conflicting_adb_servers()

        self.assertTrue(
            any(
                "Found conflicting ADB server process (PID: 2001)" in msg
                for msg in cm.output
            )
        )
        self.assertTrue(
            any(
                "Failed to kill conflicting ADB server process (PID: 2001)"
                in msg
                for msg in cm.output
            )
        )
        # Should not enter the os.kill(pid, 0) liveness poll loop when SIGKILL failed
        mock_kill.assert_called_once_with(2001, signal.SIGKILL)

    @mock.patch("os.kill", autospec=True)
    @mock.patch("subprocess.run", autospec=True)
    def test_check_for_conflicting_adb_servers_pid_only_falls_back_to_proc_cmdline(
        self, mock_run: mock.Mock, mock_kill: mock.Mock
    ) -> None:
        """Test _check_for_conflicting_adb_servers prefers /proc/<pid>/cmdline (including paths with spaces and PID-only pgrep output)."""
        self.mock_read_proc_cmdline.side_effect = self._real_read_proc_cmdline
        mock_run.return_value = subprocess.CompletedProcess(
            args=["pgrep", "-u", str(os.getuid()), "-af", "adb"],
            returncode=0,
            stdout=(
                "2001\n"
                "2002\n"
                f"2003 /tmp/dir with space/adb -P 41364 --one-device {_SERIAL_ID} server nodaemon\n"
            ),
            stderr="",
        )
        mock_kill.side_effect = [
            None,
            None,
            ProcessLookupError,
            ProcessLookupError,
        ]

        def read_bytes_side_effect(path_self: Path) -> bytes:
            if str(path_self) == "/proc/2001/cmdline":
                return (
                    f"/tmp/tmp2z9mjtqradb\x00-P\x0041363\x00--one-device\x00"
                    f"{_SERIAL_ID}\x00server\x00nodaemon\x00".encode("utf-8")
                )
            if str(path_self) == "/proc/2003/cmdline":
                return (
                    f"/tmp/dir with space/adb\x00-P\x0041364\x00--one-device\x00"
                    f"{_SERIAL_ID}\x00server\x00nodaemon\x00".encode("utf-8")
                )
            raise OSError("No such process")

        with mock.patch.object(
            Path,
            "read_bytes",
            autospec=True,
            side_effect=read_bytes_side_effect,
        ):
            with self.assertLogs(adb_server._LOGGER, level="ERROR") as cm:
                self.server._check_for_conflicting_adb_servers()

        self.assertEqual(len(cm.output), 2)
        self.assertIn("PID: 2001", cm.output[0])
        self.assertIn("PID: 2003", cm.output[1])
        self.assertEqual(
            mock_kill.call_args_list,
            [
                mock.call(2001, signal.SIGKILL),
                mock.call(2003, signal.SIGKILL),
                mock.call(2001, 0),
                mock.call(2003, 0),
            ],
        )

    @mock.patch("os.kill", autospec=True)
    @mock.patch("subprocess.run", autospec=True)
    def test_read_proc_cmdline_preserves_empty_positional_args(
        self, mock_run: mock.Mock, mock_kill: mock.Mock
    ) -> None:
        """Test _read_proc_cmdline preserves empty positional args and handles empty cmdline."""
        self.mock_read_proc_cmdline.side_effect = self._real_read_proc_cmdline
        # Set target serial to "server" so shifting "server" into the --one-device
        # slot after an empty argument would falsely trigger a conflict.
        self.server._serial_id = "server"
        mock_run.return_value = subprocess.CompletedProcess(
            args=["pgrep", "-u", str(os.getuid()), "-af", "adb"],
            returncode=0,
            stdout="2001\n2002\n",
            stderr="",
        )

        def read_bytes_side_effect(path_self: Path) -> bytes:
            if str(path_self) == "/proc/2001/cmdline":
                return b"/tmp/tmp2z9mjtqradb\x00-P\x0041363\x00--one-device\x00\x00server\x00nodaemon\x00"
            return b"\x00\x00"

        with mock.patch.object(
            Path,
            "read_bytes",
            autospec=True,
            side_effect=read_bytes_side_effect,
        ):
            self.assertEqual(
                adb_server.AdbServer._read_proc_cmdline(2001),
                [
                    "/tmp/tmp2z9mjtqradb",
                    "-P",
                    "41363",
                    "--one-device",
                    "",
                    "server",
                    "nodaemon",
                ],
            )
            self.assertEqual(adb_server.AdbServer._read_proc_cmdline(2002), [])
            self.server._check_for_conflicting_adb_servers()

        mock_kill.assert_not_called()

    @mock.patch("os.kill", autospec=True)
    @mock.patch("subprocess.run", autospec=True)
    def test_check_for_conflicting_adb_servers_different_serial_and_clients(
        self, mock_run: mock.Mock, mock_kill: mock.Mock
    ) -> None:
        """Test _check_for_conflicting_adb_servers ignores own_pids, different serials, and non-servers."""
        self.server._process = mock.Mock(pid=4001)
        # os.getpid() and 4001: own_pids (should be skipped even with conflicting args);
        # 3001: server for a different serial; 3002: adb client (not server); 3003: no command args;
        # 3004: non-adb binary matched by pgrep -af
        mock_run.return_value = subprocess.CompletedProcess(
            args=["pgrep", "-u", str(os.getuid()), "-af", "adb"],
            returncode=0,
            stdout=(
                f"{os.getpid()} /custom/adb -P 5037 server nodaemon\n"
                f"4001 /custom/adb -P 51000 --one-device {_SERIAL_ID} server nodaemon\n"
                f"3001 /custom/adb -P 51001 --one-device {_OTHER_SERIAL_ID} server nodaemon\n"
                f"3002 /custom/adb -s {_SERIAL_ID} shell echo hi\n"
                "3003\n"
                "3004 /usr/bin/python3 test_adb_runner.py server\n"
                "invalid_pid /custom/adb server\n"
            ),
            stderr="",
        )

        with mock.patch.object(adb_server._LOGGER, "error") as mock_log_error:
            self.server._check_for_conflicting_adb_servers()
            mock_log_error.assert_not_called()
        mock_kill.assert_not_called()

    @mock.patch("subprocess.run", autospec=True)
    def test_check_for_conflicting_adb_servers_pgrep_no_matches(
        self, mock_run: mock.Mock
    ) -> None:
        """Test _check_for_conflicting_adb_servers returns quietly when pgrep exits with 1."""
        # Exit code 1 means no processes matched
        mock_run.return_value = subprocess.CompletedProcess(
            args=["pgrep", "-u", str(os.getuid()), "-af", "adb"],
            returncode=1,
            stdout="",
            stderr="",
        )
        with mock.patch.object(
            adb_server._LOGGER, "warning"
        ) as mock_log_warning:
            self.server._check_for_conflicting_adb_servers()
            mock_log_warning.assert_not_called()

    @mock.patch("subprocess.run", autospec=True)
    def test_check_for_conflicting_adb_servers_pgrep_error_code(
        self, mock_run: mock.Mock
    ) -> None:
        """Test _check_for_conflicting_adb_servers logs warning when pgrep exits with error code."""
        mock_run.return_value = subprocess.CompletedProcess(
            args=["pgrep", "-u", str(os.getuid()), "-af", "adb"],
            returncode=2,
            stdout="",
            stderr="error",
        )
        with self.assertLogs(adb_server._LOGGER, level="WARNING") as cm:
            self.server._check_for_conflicting_adb_servers()
        self.assertTrue(
            any("pgrep failed with return code 2" in msg for msg in cm.output)
        )

    @mock.patch("subprocess.run", autospec=True)
    def test_check_for_conflicting_adb_servers_pgrep_exception(
        self, mock_run: mock.Mock
    ) -> None:
        """Test _check_for_conflicting_adb_servers logs warning if running pgrep raises OSError."""
        mock_run.side_effect = FileNotFoundError("pgrep not found")
        with self.assertLogs(adb_server._LOGGER, level="WARNING") as cm:
            self.server._check_for_conflicting_adb_servers()
        self.assertTrue(any("Failed to run pgrep" in msg for msg in cm.output))

    @mock.patch("time.sleep", autospec=True)
    @mock.patch("os.kill", autospec=True)
    @mock.patch("subprocess.run", autospec=True)
    def test_check_for_conflicting_adb_servers_stops_waiting_on_zombie_process(
        self,
        mock_run: mock.Mock,
        mock_kill: mock.Mock,
        mock_sleep: mock.Mock,
    ) -> None:
        """Test _check_for_conflicting_adb_servers does not spin when killed PID is a zombie."""
        mock_run.return_value = subprocess.CompletedProcess(
            args=["pgrep", "-u", str(os.getuid()), "-af", "adb"],
            returncode=0,
            stdout="2001 /custom/adb -P 5037 server nodaemon\n",
            stderr="",
        )
        # os.kill(2001, SIGKILL) and os.kill(2001, 0) both succeed because 2001 is a zombie
        mock_kill.return_value = None

        with mock.patch.object(
            Path,
            "read_text",
            return_value="2001 (tmp12345678adb) Z 1000 2001 2001 0 -1\n",
        ):
            with self.assertLogs(adb_server._LOGGER, level="ERROR"):
                self.server._check_for_conflicting_adb_servers()

        self.assertEqual(
            mock_kill.call_args_list,
            [
                mock.call(2001, signal.SIGKILL),
                mock.call(2001, 0),
            ],
        )
        mock_sleep.assert_not_called()

    @mock.patch("time.sleep", autospec=True)
    @mock.patch("os.kill", autospec=True)
    @mock.patch("subprocess.run", autospec=True)
    def test_check_for_conflicting_adb_servers_permission_error_in_wait_loop(
        self,
        mock_run: mock.Mock,
        mock_kill: mock.Mock,
        mock_sleep: mock.Mock,
    ) -> None:
        """Test _check_for_conflicting_adb_servers treats PermissionError in wait loop as exited."""
        mock_run.return_value = subprocess.CompletedProcess(
            args=["pgrep", "-u", str(os.getuid()), "-af", "adb"],
            returncode=0,
            stdout="2001 /custom/adb -P 5037 server nodaemon\n",
            stderr="",
        )
        mock_kill.side_effect = [None, PermissionError]

        with (
            mock.patch.object(
                adb_server._LOGGER, "warning"
            ) as mock_log_warning,
            self.assertLogs(adb_server._LOGGER, level="ERROR"),
        ):
            self.server._check_for_conflicting_adb_servers()
            mock_log_warning.assert_not_called()

        self.assertEqual(
            mock_kill.call_args_list,
            [
                mock.call(2001, signal.SIGKILL),
                mock.call(2001, 0),
            ],
        )
        mock_sleep.assert_not_called()

    @mock.patch("time.sleep", autospec=True)
    @mock.patch("time.monotonic", autospec=True)
    @mock.patch("os.kill", autospec=True)
    @mock.patch("subprocess.run", autospec=True)
    def test_check_for_conflicting_adb_servers_warns_when_process_survives_deadline(
        self,
        mock_run: mock.Mock,
        mock_kill: mock.Mock,
        mock_monotonic: mock.Mock,
        mock_sleep: mock.Mock,
    ) -> None:
        """Test _check_for_conflicting_adb_servers logs warning if killed PID survives 2.0s deadline."""
        mock_run.return_value = subprocess.CompletedProcess(
            args=["pgrep", "-u", str(os.getuid()), "-af", "adb"],
            returncode=0,
            stdout="2001 /custom/adb -P 5037 server nodaemon\n",
            stderr="",
        )
        mock_kill.return_value = None
        # deadline = 0.0 + 2.0; 1st poll at 0.5 (<2.0); 2nd check at 2.5 (>=2.0)
        mock_monotonic.side_effect = [0.0, 0.5, 2.5]

        with mock.patch.object(
            Path,
            "read_text",
            return_value="2001 (adb) D 1000 2001 2001 0 -1\n",
        ):
            with self.assertLogs(adb_server._LOGGER, level="WARNING") as cm:
                self.server._check_for_conflicting_adb_servers()

        mock_sleep.assert_called_once_with(0.05)
        self.assertTrue(
            any(
                "did not exit within 2.0s after SIGKILL" in msg
                and "2001" in msg
                for msg in cm.output
            )
        )

    @mock.patch("subprocess.run", autospec=True)
    @mock.patch("os.stat", autospec=True)
    @mock.patch("os.sysconf", autospec=True)
    def test_get_process_attribution_from_proc(
        self,
        mock_sysconf: mock.Mock,
        mock_stat: mock.Mock,
        mock_run: mock.Mock,
    ) -> None:
        """Test _get_process_attribution parses ppid, uid, and etime from /proc on Linux."""
        self.mock_get_process_attribution.side_effect = (
            self._real_get_process_attribution
        )
        mock_sysconf.return_value = 100
        mock_stat.return_value = mock.Mock(st_uid=1000)

        # 20 fields after ')': index 0 is state ('S'), index 1 is ppid ('555'),
        # indices 2..18 are 17 intermediate fields, index 19 is starttime ('1000').
        stat_fields_after_comm = ["S", "555"] + ["0"] * 17 + ["1000", "0"]
        proc_stat = f"2001 (adb (server)) {' '.join(stat_fields_after_comm)}\n"

        def read_text_side_effect(
            path_self: Path,
            encoding: str | None = None,
            errors: str | None = None,
        ) -> str:
            del encoding, errors
            if str(path_self) == "/proc/2001/stat":
                return proc_stat
            if str(path_self) == "/proc/uptime":
                return "125.00 400.00\n"
            raise FileNotFoundError(str(path_self))

        with (
            mock.patch.object(Path, "exists", autospec=True, return_value=True),
            mock.patch.object(
                Path,
                "read_text",
                autospec=True,
                side_effect=read_text_side_effect,
            ),
        ):
            self.assertEqual(
                adb_server.AdbServer._get_process_attribution(2001),
                "ppid=555, uid=1000, etime=115.0s",
            )
            self.assertEqual(
                adb_server.AdbServer._get_process_attribution(9999),
                "",
            )
        mock_run.assert_not_called()

    @mock.patch("subprocess.run", autospec=True)
    def test_get_process_attribution_ps_fallback(
        self, mock_run: mock.Mock
    ) -> None:
        """Test _get_process_attribution falls back to ps when /proc is unavailable."""
        self.mock_get_process_attribution.side_effect = (
            self._real_get_process_attribution
        )
        mock_run.side_effect = [
            subprocess.CompletedProcess(
                args=[],
                returncode=0,
                stdout="  444  501    02:15\n",
                stderr="",
            ),
            subprocess.CompletedProcess(
                args=[],
                returncode=1,
                stdout="",
                stderr="",
            ),
            OSError("ps failed"),
        ]

        with mock.patch.object(
            Path, "exists", autospec=True, return_value=False
        ):
            self.assertEqual(
                adb_server.AdbServer._get_process_attribution(2001),
                "ppid=444, uid=501, etime=02:15",
            )
            self.assertEqual(
                adb_server.AdbServer._get_process_attribution(2001),
                "",
            )
            self.assertEqual(
                adb_server.AdbServer._get_process_attribution(2001),
                "",
            )

        mock_run.assert_any_call(
            ["ps", "-o", "ppid=", "-o", "uid=", "-o", "etime=", "-p", "2001"],
            capture_output=True,
            text=True,
            check=False,
        )

    @mock.patch("os.kill", autospec=True)
    @mock.patch("subprocess.run", autospec=True)
    def test_check_for_conflicting_adb_servers_logs_attribution_and_peer_info(
        self, mock_run: mock.Mock, mock_kill: mock.Mock
    ) -> None:
        """Test conflict logs include attribution tags and peer-DUT servers log INFO without killing."""
        self.mock_get_process_attribution.side_effect = (
            lambda pid: f"ppid={pid - 10}, uid=1000, etime=42.0s"
        )
        mock_run.return_value = subprocess.CompletedProcess(
            args=["pgrep", "-u", str(os.getuid()), "-af", "adb"],
            returncode=0,
            stdout=(
                "1001 /custom/adb -P 5037 server nodaemon\n"
                f"1002 /custom/adb -P 51000 --one-device {_SERIAL_ID} server nodaemon\n"
                f"1003 /custom/adb -P 51001 --one-device {_OTHER_SERIAL_ID} server nodaemon\n"
            ),
            stderr="",
        )
        mock_kill.side_effect = [
            None,
            None,
            ProcessLookupError,
            ProcessLookupError,
        ]

        with self.assertLogs(adb_server._LOGGER, level="INFO") as cm:
            self.server._check_for_conflicting_adb_servers()

        self.assertTrue(
            any(
                record.levelname == "ERROR"
                and "[UNSCOPED]" in record.getMessage()
                and "PID: 1001, ppid=991, uid=1000, etime=42.0s"
                in record.getMessage()
                for record in cm.records
            )
        )
        self.assertTrue(
            any(
                record.levelname == "ERROR"
                and "[ACTIVE DUT]" in record.getMessage()
                and "PID: 1002, ppid=992, uid=1000, etime=42.0s"
                in record.getMessage()
                for record in cm.records
            )
        )
        peer_records = [
            record for record in cm.records if "1003" in record.getMessage()
        ]
        self.assertEqual(len(peer_records), 1)
        self.assertEqual(peer_records[0].levelname, "INFO")
        self.assertIn(
            "[PEER DEVICE - IGNORED] ADB server process (PID: 1003, ppid=993, uid=1000, etime=42.0s)",
            peer_records[0].getMessage(),
        )
        self.assertEqual(
            mock_kill.call_args_list,
            [
                mock.call(1001, signal.SIGKILL),
                mock.call(1002, signal.SIGKILL),
                mock.call(1001, 0),
                mock.call(1002, 0),
            ],
        )

    @mock.patch("os.getppid", autospec=True)
    def test_run_server_retries_until_max_restarts(
        self, mock_getppid: mock.Mock
    ) -> None:
        """Test _run_server retries _run_one_server up to _MAX_SERVER_RESTARTS then raises RuntimeError."""
        # getppid() != 1 ensures _run_server does not exit early as an orphaned process
        mock_getppid.return_value = 1234

        with (
            mock.patch.object(
                self.server, "_run_one_server"
            ) as mock_run_one_server,
            mock.patch.object(
                self.server._server_stop_requested, "wait"
            ) as mock_wait,
        ):
            with self.assertRaisesRegex(
                RuntimeError,
                "ADB server failed to start multiple times. Giving up.",
            ):
                self.server._run_server()

            self.assertEqual(
                mock_run_one_server.call_args_list,
                [
                    mock.call("adb_server_1"),
                    mock.call("adb_server_2"),
                    mock.call("adb_server_3"),
                ],
            )
            self.assertEqual(
                self.server._attempt_count,
                adb_server.AdbServer._MAX_SERVER_RESTARTS,
            )
            self.assertEqual(
                self.server._server_id,
                adb_server.AdbServer._MAX_SERVER_RESTARTS,
            )
            # Waits 1s before the 2nd and 3rd attempts (not before the 1st)
            self.assertEqual(mock_wait.call_count, 2)

    @mock.patch("os.getppid", autospec=True)
    def test_run_server_exits_when_stop_requested_after_first_run(
        self, mock_getppid: mock.Mock
    ) -> None:
        """Test _run_server exits loop cleanly without retrying when stop is requested."""
        mock_getppid.return_value = 1234

        def stop_during_first_run(_: str) -> None:
            self.server._server_stop_requested.set()

        with (
            mock.patch.object(
                self.server,
                "_run_one_server",
                side_effect=stop_during_first_run,
            ) as mock_run_one_server,
            mock.patch.object(
                self.server._server_stop_requested, "wait"
            ) as mock_wait,
        ):
            self.server._run_server()

            mock_run_one_server.assert_called_once_with("adb_server_1")
            self.assertEqual(self.server._attempt_count, 1)
            self.assertEqual(self.server._server_id, 1)
            mock_wait.assert_not_called()

    @mock.patch("os.getppid", autospec=True)
    def test_run_server_exits_if_orphaned(
        self, mock_getppid: mock.Mock
    ) -> None:
        """Test _run_server exits loop immediately if parent process died (ppid == 1)."""
        mock_getppid.return_value = 1

        with mock.patch.object(
            self.server, "_run_one_server"
        ) as mock_run_one_server:
            self.server._run_server()
            mock_run_one_server.assert_not_called()

    @mock.patch("os.getppid", autospec=True)
    def test_run_server_reraises_exception_from_run_one_server(
        self, mock_getppid: mock.Mock
    ) -> None:
        """Test _run_server propagates exceptions raised by _run_one_server."""
        mock_getppid.return_value = 1234

        with mock.patch.object(
            self.server, "_run_one_server"
        ) as mock_run_one_server:
            mock_run_one_server.side_effect = adb_errors.AdbServerError(
                "bind failed"
            )
            with self.assertRaisesRegex(
                adb_errors.AdbServerError, "bind failed"
            ):
                self.server._run_server()

    @mock.patch("time.sleep", autospec=True)
    @mock.patch("subprocess.Popen", autospec=True)
    @mock.patch("socket.socket", autospec=True)
    def test_run_one_server_ephemeral_port_allocation_and_poll_retry(
        self,
        mock_socket_cls: mock.Mock,
        mock_popen_cls: mock.Mock,
        mock_sleep: mock.Mock,
    ) -> None:
        """Test _run_one_server allocates an ephemeral port, retries connect_ex until open, and cleans up."""
        mock_sock = mock.MagicMock()
        mock_sock.__enter__.return_value = mock_sock
        mock_sock.getsockname.return_value = (mock.ANY, _EPHEMERAL_PORT)
        # First poll returns ECONNREFUSED, second poll succeeds (0)
        mock_sock.connect_ex.side_effect = [errno.ECONNREFUSED, 0]
        mock_socket_cls.return_value = mock_sock

        mock_proc = mock.Mock()
        mock_proc.returncode = 0
        # Process has already exited when finally block checks poll()
        mock_proc.poll.return_value = 0

        def wait_side_effect() -> int:
            # Verify server is marked ready while waiting on process
            self.assertTrue(self.server._server_is_ready.is_set())
            self.assertEqual(self.server._server_port, _EPHEMERAL_PORT)
            self.server._server_stop_requested.set()
            return 0

        mock_proc.wait.side_effect = wait_side_effect
        mock_popen_cls.return_value = mock_proc

        with mock.patch.object(
            self.server, "_check_for_conflicting_adb_servers"
        ):
            self.server._run_one_server("adb_server_1")

        mock_sock.bind.assert_called_once_with(("", 0))
        self.assertEqual(mock_sock.connect_ex.call_count, 2)
        mock_sleep.assert_called_once_with(0.1)

        mock_popen_cls.assert_called_once()
        popen_args, popen_kwargs = mock_popen_cls.call_args
        self.assertEqual(
            popen_args[0],
            [
                _ADB_BINARY,
                "-P",
                str(_EPHEMERAL_PORT),
                "--one-device",
                _SERIAL_ID,
                "server",
                "nodaemon",
            ],
        )
        self.assertEqual(popen_kwargs["env"]["ADB_TRACE"], "all")
        self.assertEqual(popen_kwargs["env"]["ADB_MDNS_OPENSCREEN"], "0")
        self.assertIs(
            popen_kwargs["preexec_fn"],
            adb_server._set_pdeathsig if sys.platform == "linux" else None,
        )

        # Finally block resets _process and _server_port so subsequent runs pick a new port
        self.assertIsNone(self.server._process)
        self.assertIsNone(self.server._server_port)
        self.assertFalse(self.server._server_is_ready.is_set())

    @mock.patch("time.sleep", autospec=True)
    @mock.patch("time.time", autospec=True)
    @mock.patch("subprocess.Popen", autospec=True)
    @mock.patch("socket.socket", autospec=True)
    def test_run_one_server_port_collision_timeout_kills_leaked_process(
        self,
        mock_socket_cls: mock.Mock,
        mock_popen_cls: mock.Mock,
        mock_time: mock.Mock,
        mock_sleep: mock.Mock,
    ) -> None:
        """Test _run_one_server raises AdbServerError on port bind timeout and kills leaked process."""
        mock_sock = mock.MagicMock()
        mock_sock.__enter__.return_value = mock_sock
        mock_sock.getsockname.return_value = (mock.ANY, _EPHEMERAL_PORT)
        mock_sock.connect_ex.return_value = errno.ECONNREFUSED
        mock_socket_cls.return_value = mock_sock

        # In the 5s port-polling loop, time.time() calls:
        # start_time=0.0, 1st poll=1.0 (<5s), 2nd check=6.0 (>=5s timeout)
        mock_time.side_effect = [0.0, 1.0, 6.0]

        mock_proc = mock.Mock()
        # poll() returning None means the spawned process is still running in the finally block
        mock_proc.poll.return_value = None
        # 1st wait(timeout=3) after SIGTERM times out; 2nd wait() after SIGKILL succeeds
        mock_proc.wait.side_effect = [
            subprocess.TimeoutExpired(cmd="adb", timeout=3),
            0,
        ]
        mock_popen_cls.return_value = mock_proc

        with mock.patch.object(
            self.server, "_check_for_conflicting_adb_servers"
        ):
            with self.assertRaisesRegex(
                adb_errors.AdbServerError,
                f"ADB server failed to bind to port {_EPHEMERAL_PORT} within 5 seconds",
            ):
                self.server._run_one_server("adb_server_1")

        mock_sleep.assert_called_once_with(0.1)

        # Verify leaked process was terminated and then forcefully killed on timeout
        mock_proc.terminate.assert_called_once_with()
        mock_proc.kill.assert_called_once_with()
        self.assertIsNone(self.server._process)
        self.assertIsNone(self.server._server_port)

    @mock.patch("subprocess.Popen", autospec=True)
    def test_run_one_server_aborts_if_stop_requested_before_popen(
        self, mock_popen_cls: mock.Mock
    ) -> None:
        """Test _run_one_server returns early if stop is requested before starting process."""
        self.server._server_stop_requested.set()

        with mock.patch.object(
            self.server, "_check_for_conflicting_adb_servers"
        ):
            self.server._run_one_server("adb_server_1")

        mock_popen_cls.assert_not_called()

    @mock.patch("subprocess.Popen", autospec=True)
    @mock.patch("socket.socket", autospec=True)
    def test_run_one_server_aborts_if_stop_requested_during_port_polling(
        self,
        mock_socket_cls: mock.Mock,
        mock_popen_cls: mock.Mock,
    ) -> None:
        """Test _run_one_server returns early if stop is requested while polling the port."""
        mock_sock = mock.MagicMock()
        mock_sock.__enter__.return_value = mock_sock
        mock_sock.getsockname.return_value = (mock.ANY, _EPHEMERAL_PORT)

        # Trigger stop request during the 1st port poll (after Popen has already started)
        def request_stop_on_connect(_: tuple[str, int]) -> int:
            self.server._server_stop_requested.set()
            return errno.ECONNREFUSED

        mock_sock.connect_ex.side_effect = request_stop_on_connect
        mock_socket_cls.return_value = mock_sock

        mock_proc = mock.Mock()
        mock_proc.poll.return_value = 0
        mock_popen_cls.return_value = mock_proc

        with mock.patch.object(
            self.server, "_check_for_conflicting_adb_servers"
        ):
            self.server._run_one_server("adb_server_1")

        self.assertFalse(self.server._server_is_ready.is_set())
        self.assertIsNone(self.server._process)
        self.assertIsNone(self.server._server_port)

    @mock.patch("subprocess.Popen", autospec=True)
    @mock.patch("socket.socket", autospec=True)
    def test_run_one_server_sets_vendor_keys_env(
        self,
        mock_socket_cls: mock.Mock,
        mock_popen_cls: mock.Mock,
    ) -> None:
        """Test _run_one_server sets ADB_VENDOR_KEYS when vendor_keys_path is provided."""
        mock_sock = mock.MagicMock()
        mock_sock.__enter__.return_value = mock_sock
        mock_sock.getsockname.return_value = (mock.ANY, _EPHEMERAL_PORT)
        mock_sock.connect_ex.return_value = 0
        mock_socket_cls.return_value = mock_sock

        mock_proc = mock.Mock()
        mock_proc.returncode = 0
        mock_proc.poll.return_value = 0
        mock_popen_cls.return_value = mock_proc

        server_with_keys = adb_server.AdbServer(
            adb_binary_path=_ADB_BINARY,
            serial_id=_SERIAL_ID,
            output_dir=self.output_dir,
            vendor_keys_path=_VENDOR_KEYS_PATH,
        )
        with mock.patch.object(
            server_with_keys, "_check_for_conflicting_adb_servers"
        ):
            server_with_keys._run_one_server("adb_server_1")

        _, popen_kwargs = mock_popen_cls.call_args
        self.assertEqual(
            popen_kwargs["env"]["ADB_VENDOR_KEYS"], _VENDOR_KEYS_PATH
        )

    @mock.patch("subprocess.Popen", autospec=True)
    @mock.patch("socket.socket", autospec=True)
    def test_run_one_server_strips_inherited_vendor_keys_env(
        self,
        mock_socket_cls: mock.Mock,
        mock_popen_cls: mock.Mock,
    ) -> None:
        """Test _run_one_server strips inherited ADB_VENDOR_KEYS when vendor_keys_path is None."""
        mock_sock = mock.MagicMock()
        mock_sock.__enter__.return_value = mock_sock
        mock_sock.getsockname.return_value = (mock.ANY, _EPHEMERAL_PORT)
        mock_sock.connect_ex.return_value = 0
        mock_socket_cls.return_value = mock_sock

        mock_proc = mock.Mock()
        mock_proc.returncode = 0
        mock_proc.poll.return_value = 0
        mock_popen_cls.return_value = mock_proc

        with (
            mock.patch.dict(
                "os.environ", {"ADB_VENDOR_KEYS": _VENDOR_KEYS_PATH}
            ),
            mock.patch.object(
                self.server, "_check_for_conflicting_adb_servers"
            ),
        ):
            self.server._run_one_server("adb_server_1")

        _, popen_kwargs = mock_popen_cls.call_args
        self.assertNotIn("ADB_VENDOR_KEYS", popen_kwargs["env"])

    def test_start_and_stop_with_fake_adb(self) -> None:
        """Test end-to-end AdbServer lifecycle with a fake adb script.

        Verifies that:
        1. A pre-existing conflicting `adb ... server` process is killed and
           reaped during `start()`.
        2. The new `AdbServer` instance binds to its allocated port.
        3. `stop()` terminates and reaps the server process leaving nothing
           running.
        """
        self.mock_read_proc_cmdline.side_effect = self._real_read_proc_cmdline
        fake_interp = self.output_dir / "python_adb"
        fake_interp.symlink_to(os.path.realpath(sys.executable))
        fake_adb = self.output_dir / "adb"
        fake_adb.write_text(
            f"#!{fake_interp}\n"
            "import signal\n"
            "import socket\n"
            "import sys\n"
            "\n"
            "port = int(sys.argv[sys.argv.index('-P') + 1])\n"
            "sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)\n"
            "sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)\n"
            "sock.bind(('127.0.0.1', port))\n"
            "sock.listen(1)\n"
            "signal.pause()\n",
            encoding="utf-8",
        )
        fake_adb.chmod(0o755)

        serial_id = f"{_SERIAL_ID}_{os.getpid()}"
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
            sock.bind(("127.0.0.1", 0))
            old_port = sock.getsockname()[1]

        # 1. Spawn a fake pre-existing conflicting `adb ... server` process.
        old_proc = subprocess.Popen(
            [
                str(fake_adb),
                "-P",
                str(old_port),
                "--one-device",
                serial_id,
                "server",
                "nodaemon",
            ]
        )

        def cleanup_old_proc() -> None:
            if old_proc.poll() is None:
                old_proc.kill()
                old_proc.wait()

        self.addCleanup(cleanup_old_proc)

        # Wait on old_proc in a background thread so the kernel reaps it when
        # _check_for_conflicting_adb_servers() sends SIGKILL, matching how an
        # external/daemon process is reaped rather than remaining a zombie child
        # of this test process.
        reaper = threading.Thread(target=old_proc.wait, daemon=True)
        reaper.start()

        deadline = time.monotonic() + 5.0
        while time.monotonic() < deadline:
            with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
                if sock.connect_ex(("127.0.0.1", old_port)) == 0:
                    break
            time.sleep(0.05)
        else:
            self.fail("Pre-existing fake adb server failed to bind port")

        # 2. Start AdbServer -> assert the old one is killed and reaped, and
        # ours is bound.
        server = adb_server.AdbServer(
            adb_binary_path=str(fake_adb),
            serial_id=serial_id,
            output_dir=self.output_dir,
        )
        self.addCleanup(server.stop)
        server.start()

        reaper.join(timeout=1.0)
        # On POSIX, Popen.poll() returns -N when terminated by signal N.
        self.assertEqual(old_proc.poll(), -signal.SIGKILL)
        with self.assertRaises(ProcessLookupError):
            os.kill(old_proc.pid, 0)

        self.assertIsNotNone(server._process)
        assert server._process is not None
        new_pid = server._process.pid
        new_port = server.port()
        self.assertIsNone(server._process.poll())
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
            self.assertEqual(sock.connect_ex((server.host(), new_port)), 0)

        # 3. Stop AdbServer -> assert nothing is left running.
        server.stop()

        self.assertIsNone(server._process)
        self.assertIsNone(server._server_thread)
        with self.assertRaises(ProcessLookupError):
            os.kill(new_pid, 0)
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
            self.assertNotEqual(sock.connect_ex((server.host(), new_port)), 0)

        pgrep_result = subprocess.run(
            ["pgrep", "-a", "adb"],
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertNotIn(serial_id, pgrep_result.stdout)


if __name__ == "__main__":
    unittest.main()
