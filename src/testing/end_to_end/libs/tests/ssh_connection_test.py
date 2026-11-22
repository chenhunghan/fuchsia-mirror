#!/usr/bin/env fuchsia-vendored-python
#
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import gc
import subprocess
import unittest
from unittest import mock

from libs.proc.runner import CalledProcessTransportError
from libs.ssh.connection import SshConnection
from libs.ssh.settings import SshSettings


class SshConnectionReachabilityTest(unittest.TestCase):
    def setUp(self) -> None:
        self.settings = SshSettings(
            hostname="192.168.42.11",
            username="root",
            identity_file="/tmp/id_rsa",
            connect_timeout=10,
        )
        self.conn = SshConnection(self.settings)

    def tearDown(self) -> None:
        del self.conn
        gc.collect()

    @mock.patch("libs.ssh.connection.wait_for_port")
    @mock.patch.object(SshConnection, "run")
    @mock.patch.object(SshConnection, "close")
    def test_wait_until_reachable_success(
        self,
        mock_close: mock.Mock,
        mock_run: mock.Mock,
        mock_wait_for_port: mock.Mock,
    ) -> None:
        mock_run.return_value = subprocess.CompletedProcess(
            args=["echo"], returncode=0, stdout=b"\n", stderr=b""
        )
        self.conn.wait_until_reachable(timeout_sec=15)

        mock_wait_for_port.assert_called_once_with(
            "192.168.42.11", 22, timeout_sec=5
        )
        mock_close.assert_called_once()
        mock_run.assert_called_once()

    @mock.patch("libs.ssh.connection.time.sleep")
    @mock.patch("libs.ssh.connection.wait_for_port")
    @mock.patch.object(SshConnection, "run")
    @mock.patch.object(SshConnection, "close")
    def test_wait_until_reachable_retries_on_port_and_ssh_errors(
        self,
        mock_close: mock.Mock,
        mock_run: mock.Mock,
        mock_wait_for_port: mock.Mock,
        _mock_sleep: mock.Mock,
    ) -> None:
        mock_wait_for_port.side_effect = [TimeoutError("port down"), None]
        mock_run.side_effect = [
            CalledProcessTransportError("ssh not ready"),
            subprocess.CompletedProcess(
                args=["echo"], returncode=0, stdout=b"\n", stderr=b""
            ),
        ]

        self.conn.wait_until_reachable(timeout_sec=15)

        self.assertEqual(mock_wait_for_port.call_count, 2)
        self.assertEqual(mock_close.call_count, 2)
        self.assertEqual(mock_run.call_count, 2)

    @mock.patch("libs.ssh.connection.time.monotonic")
    @mock.patch("libs.ssh.connection.wait_for_port")
    @mock.patch.object(SshConnection, "run")
    @mock.patch.object(SshConnection, "close")
    def test_wait_until_reachable_raises_timeout_error_on_ssh_failure(
        self,
        mock_close: mock.Mock,
        mock_run: mock.Mock,
        mock_wait_for_port: mock.Mock,
        mock_monotonic: mock.Mock,
    ) -> None:
        mock_monotonic.side_effect = [100.0, 100.0, 105.0, 116.0]
        mock_wait_for_port.return_value = None
        mock_run.side_effect = CalledProcessTransportError("ssh not ready")

        with self.assertRaises(TimeoutError) as ctx:
            self.conn.wait_until_reachable(timeout_sec=15)

        self.assertIsInstance(
            ctx.exception.__cause__, CalledProcessTransportError
        )
        mock_close.assert_called_once()
        mock_run.assert_called_once()

    @mock.patch("libs.ssh.connection.wait_for_port")
    @mock.patch.object(SshConnection, "close")
    def test_wait_until_unreachable_success(
        self,
        mock_close: mock.Mock,
        mock_wait_for_port: mock.Mock,
    ) -> None:
        mock_wait_for_port.side_effect = TimeoutError("port closed")

        self.conn.wait_until_unreachable(interval_sec=1, timeout_sec=5)

        mock_close.assert_called_once()
        mock_wait_for_port.assert_called_once_with(
            "192.168.42.11", 22, timeout_sec=1
        )

    @mock.patch("libs.ssh.connection.time.sleep")
    @mock.patch("libs.ssh.connection.os.path.exists", return_value=False)
    @mock.patch("libs.ssh.connection.job.run_async")
    def test_setup_main_ssh_exits_early_when_process_terminates(
        self,
        mock_run_async: mock.Mock,
        _mock_exists: mock.Mock,
        mock_sleep: mock.Mock,
    ) -> None:
        mock_proc = mock.Mock()
        mock_proc.poll.return_value = 255
        mock_run_async.return_value = mock_proc

        with self.assertRaises(CalledProcessTransportError):
            self.conn.setup_main_ssh(timeout_sec=30)

        mock_sleep.assert_not_called()
        self.assertIsNone(self.conn._main_ssh_proc)

    @mock.patch("libs.ssh.connection.time.time")
    @mock.patch("libs.ssh.connection.job.run")
    @mock.patch.object(SshConnection, "setup_main_ssh")
    def test_run_shares_deadline_between_setup_main_ssh_and_job_run(
        self,
        mock_setup_main_ssh: mock.Mock,
        mock_job_run: mock.Mock,
        mock_time: mock.Mock,
    ) -> None:
        mock_time.side_effect = [100.0, 100.0, 103.0]
        mock_setup_main_ssh.side_effect = CalledProcessTransportError(
            "main ssh failed"
        )
        mock_job_run.return_value = subprocess.CompletedProcess(
            args=["ssh", "echo"], returncode=0, stdout=b"\n", stderr=b""
        )

        self.conn.run(["echo"], timeout_sec=8.0, attempts=1)

        mock_setup_main_ssh.assert_called_once_with(8.0)
        self.assertEqual(mock_job_run.call_args.kwargs["timeout_sec"], 5.0)


if __name__ == "__main__":
    unittest.main()
