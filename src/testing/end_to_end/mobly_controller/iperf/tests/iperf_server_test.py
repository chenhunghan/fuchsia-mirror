# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from __future__ import annotations

import json
import unittest
from unittest import mock

from iperf import iperf_server
from iperf.iperf_server import (
    IPerfResult,
    IPerfServer,
    IPerfServerOverSsh,
    _get_port_from_ss_output,
)
from libs.ssh import settings
from libs.types import ControllerConfig

MOCK_LOGFILE_PATH = "/tmp/mock_iperf_log.log"


class IPerfServerModuleTest(unittest.TestCase):
    """Tests the iperf_server module factory and port detection functions."""

    def test_create_creates_local_iperf_server_with_int(self) -> None:
        servers = iperf_server.create([5201])  # type: ignore[list-item]
        self.assertEqual(len(servers), 1)
        self.assertIsInstance(servers[0], IPerfServer)
        self.assertEqual(servers[0].port, 5201)

    def test_create_creates_local_iperf_server_with_str(self) -> None:
        servers = iperf_server.create(["5201"])  # type: ignore[list-item]
        self.assertEqual(len(servers), 1)
        self.assertIsInstance(servers[0], IPerfServer)
        self.assertEqual(servers[0].port, 5201)

    def test_create_cannot_create_local_iperf_server_with_bad_str(self) -> None:
        with self.assertRaises(ValueError):
            iperf_server.create(["not_a_port"])  # type: ignore[list-item]

    @mock.patch("libs.ssh.connection.SshConnection")
    @mock.patch("antlion.utils.get_interface_based_on_ip", return_value="eth1")
    @mock.patch(
        "libs.commands.command.LinuxCommand.available", return_value=False
    )
    def test_create_creates_server_over_ssh_with_ssh_config(
        self, _mock_cmd: mock.Mock, _mock_net: mock.Mock, _mock_ssh: mock.Mock
    ) -> None:
        cfg: ControllerConfig = {
            "ssh_config": {
                "user": "root",
                "host": "192.168.1.1",
                "identity_file": "/dev/null",
            },
            "port": 5201,
            "test_interface": "lan",
        }
        servers = iperf_server.create([cfg])
        self.assertEqual(len(servers), 1)
        server = servers[0]
        self.assertIsInstance(server, IPerfServerOverSsh)
        assert isinstance(server, IPerfServerOverSsh)
        self.assertEqual(server.port, 5201)
        self.assertEqual(server.test_interface, "lan")

    def test_create_raises_value_error_on_invalid_config(self) -> None:
        with self.assertRaises(ValueError):
            iperf_server.create([{"invalid_key": "val"}])

    def test_get_port_from_ss_output_returns_correct_port_ipv4(self) -> None:
        ss_output = (
            "tcp LISTEN  0 5 127.0.0.1:5201  *:*"
            ' users:(("iperf3",pid=1234,fd=3))\n'
        )
        self.assertEqual(_get_port_from_ss_output(ss_output, 1234), "5201")

    def test_get_port_from_ss_output_returns_correct_port_ipv6(self) -> None:
        ss_output = (
            "tcp LISTEN  0 5 [::]:5201  *:*"
            ' users:(("iperf3",pid=1234,fd=3))\n'
        )
        self.assertEqual(_get_port_from_ss_output(ss_output, 1234), "5201")

    def test_get_port_from_ss_output_raises_when_not_found(self) -> None:
        ss_output = 'tcp LISTEN  0 5 127.0.0.1:5201  *:* users:(("iperf3",pid=9999,fd=3))\n'
        with self.assertRaises(ProcessLookupError):
            _get_port_from_ss_output(ss_output, 1234)

    def test_destroy_calls_stop(self) -> None:
        mock_server = mock.create_autospec(IPerfServerOverSsh)
        iperf_server.destroy([mock_server])
        mock_server.stop.assert_called_once()


class IPerfResultTest(unittest.TestCase):
    """Tests IPerfResult parsing."""

    def test_parse_valid_json(self) -> None:
        json_data = (
            '{"end": {"sum_received": {"bits_per_second": 8000000},'
            ' "sum_sent": {"bits_per_second": 8000000},'
            ' "sum": {"bits_per_second": 8000000}},'
            ' "intervals": [{"sum": {"bits_per_second": 8000000}}]}'
        )
        result = IPerfResult(json_data, reporting_speed_units="Mbits")
        self.assertIsNotNone(result.avg_rate)
        self.assertIsNotNone(result.avg_receive_rate)
        self.assertIsNotNone(result.avg_send_rate)
        expected_rate = 8000000 / (1024 * 1024)
        self.assertAlmostEqual(result.avg_rate or 0.0, expected_rate, places=2)
        self.assertAlmostEqual(
            result.avg_receive_rate or 0.0, expected_rate, places=2
        )
        self.assertAlmostEqual(
            result.avg_send_rate or 0.0, expected_rate, places=2
        )

    def test_std_deviation_calculation(self) -> None:
        """Verifies sample standard deviation (N-1) calculation using math.fsum."""
        mb_to_bps = 1024 * 1024 * 8
        mock_result = {
            "end": {"sum": {"bits_per_second": 10 * mb_to_bps}},
            "intervals": [
                {"sum": {"bits_per_second": 10 * mb_to_bps}},
                {"sum": {"bits_per_second": 20 * mb_to_bps}},
                {"sum": {"bits_per_second": 30 * mb_to_bps}},
                {
                    "sum": {"bits_per_second": 999 * mb_to_bps}
                },  # ignored (last interval)
            ],
        }
        res = IPerfResult(json.dumps(mock_result))
        std_dev = res.get_std_deviation(iperf_ignored_interval=0)
        self.assertIsNotNone(std_dev)
        assert std_dev is not None
        self.assertAlmostEqual(std_dev, 10.0, places=4)


class IPerfServerLocalTest(unittest.TestCase):
    """Tests local IPerfServer execution."""

    def setUp(self) -> None:
        patcher = mock.patch.object(
            iperf_server, "_get_port_from_ss_output", return_value="5201"
        )
        patcher.start()
        self.addCleanup(patcher.stop)

    @mock.patch("builtins.open")
    @mock.patch("subprocess.Popen")
    @mock.patch("libs.proc.job.run")
    def test_start_and_stop(
        self, mock_job: mock.Mock, mock_popen: mock.Mock, _mock_open: mock.Mock
    ) -> None:
        mock_proc = mock.Mock()
        mock_proc.pid = 1234
        mock_popen.return_value = mock_proc
        mock_job.return_value.stdout = b"fake ss output"

        server = IPerfServer(5201)
        with mock.patch.object(
            server, "_get_full_file_path", return_value=MOCK_LOGFILE_PATH
        ):
            server.start()
            self.assertTrue(server.started)

            log_path = server.stop()
            self.assertFalse(server.started)
            self.assertEqual(log_path, MOCK_LOGFILE_PATH)
            mock_proc.terminate.assert_called_once()


class IPerfServerOverSshTest(unittest.TestCase):
    """Tests remote IPerfServerOverSsh execution."""

    def setUp(self) -> None:
        patcher_net = mock.patch(
            "antlion.utils.get_interface_based_on_ip", return_value="eth1"
        )
        patcher_cmd = mock.patch(
            "libs.commands.command.LinuxCommand.available", return_value=False
        )
        patcher_net.start()
        patcher_cmd.start()
        self.addCleanup(patcher_net.stop)
        self.addCleanup(patcher_cmd.stop)

    def _create_ssh_settings(self, user: str = "root") -> settings.SshSettings:
        return settings.from_config(
            {
                "host": "192.168.42.11",
                "user": user,
                "identity_file": "/dev/null",
            }
        )

    @mock.patch("libs.ssh.connection.SshConnection")
    def test_start_makes_started_true(self, mock_conn: mock.Mock) -> None:
        ssh_cfg = self._create_ssh_settings()
        server = IPerfServerOverSsh(ssh_cfg, 5201, "eth0")
        server._ssh_session = mock.Mock()
        with (
            mock.patch.object(server, "_cleanup_iperf_port"),
            mock.patch.object(
                server, "_get_full_file_path", return_value=MOCK_LOGFILE_PATH
            ),
        ):
            server.start()
            self.assertTrue(server.started)

    @mock.patch("builtins.open")
    @mock.patch("libs.ssh.connection.SshConnection")
    def test_start_stop_makes_started_false(
        self, mock_conn: mock.Mock, _mock_open: mock.Mock
    ) -> None:
        ssh_cfg = self._create_ssh_settings()
        server = IPerfServerOverSsh(ssh_cfg, 5201, "eth0")
        server._ssh_session = mock.Mock()
        with (
            mock.patch.object(server, "_cleanup_iperf_port"),
            mock.patch.object(
                server, "_get_full_file_path", return_value=MOCK_LOGFILE_PATH
            ),
        ):
            server.start()
            server.stop()
            self.assertFalse(server.started)

    @mock.patch("builtins.open")
    @mock.patch("libs.ssh.connection.SshConnection")
    def test_stop_returns_expected_log_file(
        self, mock_conn: mock.Mock, _mock_open: mock.Mock
    ) -> None:
        ssh_cfg = self._create_ssh_settings()
        server = IPerfServerOverSsh(ssh_cfg, 5201, "eth0")
        server._ssh_session = mock.Mock()
        server._iperf_pid = "1234"
        with (
            mock.patch.object(server, "_cleanup_iperf_port"),
            mock.patch.object(
                server, "_get_full_file_path", return_value=MOCK_LOGFILE_PATH
            ),
        ):
            log_file = server.stop()
            self.assertEqual(log_file, MOCK_LOGFILE_PATH)

    @mock.patch("libs.ssh.connection.SshConnection")
    def test_start_does_not_run_two_concurrent_processes(
        self, mock_conn: mock.Mock
    ) -> None:
        ssh_cfg = self._create_ssh_settings()
        server = IPerfServerOverSsh(ssh_cfg, 5201, "eth0")
        server._ssh_session = mock.Mock()
        server._iperf_pid = "1234"
        with (
            mock.patch.object(server, "_cleanup_iperf_port"),
            mock.patch.object(
                server, "_get_full_file_path", return_value=MOCK_LOGFILE_PATH
            ),
        ):
            server.start()
            self.assertFalse(server._ssh_session.run_async.called)

    @mock.patch("libs.ssh.connection.SshConnection")
    def test_stop_exits_early_if_no_process_has_started(
        self, mock_conn: mock.Mock
    ) -> None:
        ssh_cfg = self._create_ssh_settings()
        server = IPerfServerOverSsh(ssh_cfg, 5201, "eth0")
        server._ssh_session = mock.Mock()
        server._iperf_pid = None
        with (
            mock.patch.object(server, "_cleanup_iperf_port"),
            mock.patch.object(
                server, "_get_full_file_path", return_value=MOCK_LOGFILE_PATH
            ),
        ):
            server.stop()
            self.assertFalse(server._ssh_session.run_async.called)


if __name__ == "__main__":
    unittest.main()
