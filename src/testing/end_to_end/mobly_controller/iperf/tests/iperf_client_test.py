# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from __future__ import annotations

import os
import unittest
from unittest import mock

from iperf import iperf_client
from iperf.iperf_client import (
    IPerfClient,
    IPerfClientBase,
    IPerfClientOverAdb,
    IPerfClientOverSsh,
)
from libs.types import ControllerConfig

ARGS = 0
KWARGS = 1
MOCK_LOGFILE_PATH = "/path/to/foo"


class IPerfClientModuleTest(unittest.TestCase):
    """Tests the iperf.iperf_client module functions."""

    @mock.patch("iperf.iperf_client.SSHProvider")
    def test_create_can_create_client_over_ssh(
        self, _mock_ssh_provider: mock.Mock
    ) -> None:
        cfg: ControllerConfig = {
            "ssh_config": {
                "user": "root",
                "host": "192.168.42.11",
                "identity_file": "/dev/null",
            },
            "test_interface": "wlan0",
            "sync_date": False,
        }
        clients = iperf_client.create([cfg])
        self.assertEqual(len(clients), 1)
        self.assertIsInstance(clients[0], IPerfClientOverSsh)
        self.assertEqual(clients[0].test_interface, "wlan0")

    def test_create_can_create_local_client(self) -> None:
        clients = iperf_client.create([{}])
        self.assertEqual(len(clients), 1)
        self.assertIsInstance(clients[0], IPerfClient)


class IPerfClientBaseTest(unittest.TestCase):
    """Tests iperf.iperf_client.IPerfClientBase."""

    @mock.patch("antlion.context.get_current_context")
    @mock.patch("os.makedirs")
    def test_get_full_file_path_creates_parent_directory(
        self, mock_makedirs: mock.Mock, mock_get_context: mock.Mock
    ) -> None:
        mock_get_context.return_value.get_full_output_path.return_value = (
            "/tmp/unit_test_garbage"
        )

        full_file_path = IPerfClientBase._get_full_file_path("0")

        self.assertTrue(
            mock_makedirs.called, "Did not attempt to create a directory."
        )
        self.assertEqual(
            os.path.dirname(full_file_path),
            mock_makedirs.call_args[ARGS][0],
            "The parent directory of the full file path was not created.",
        )


class IPerfClientTest(unittest.TestCase):
    """Tests iperf.iperf_client.IPerfClient."""

    @mock.patch("builtins.open")
    @mock.patch("subprocess.call")
    def test_start_writes_to_full_file_path(
        self, mock_call: mock.Mock, mock_open: mock.Mock
    ) -> None:
        client = IPerfClient()
        file_path = "/path/to/foo"
        with mock.patch.object(
            client, "_get_full_file_path", return_value=file_path
        ):
            client.start("127.0.0.1", "IPERF_ARGS", "TAG")

        mock_open.assert_called_with(file_path, "w")
        self.assertEqual(
            mock_call.call_args[KWARGS]["stdout"],
            mock_open().__enter__.return_value,
            "IPerfClient did not write the logs to the expected file.",
        )


class IPerfClientOverSshTest(unittest.TestCase):
    """Tests iperf.iperf_client.IPerfClientOverSsh."""

    @mock.patch("builtins.open")
    def test_start_writes_output_to_full_file_path(
        self,
        mock_open: mock.Mock,
    ) -> None:
        mock_ssh_provider = mock.Mock()
        mock_process = mock.Mock()
        mock_process.stdout = b"iperf test output"
        mock_ssh_provider.run.return_value = mock_process

        client = IPerfClientOverSsh(mock_ssh_provider, sync_date=False)

        file_path = "/path/to/foo"
        with mock.patch.object(
            client, "_get_full_file_path", return_value=file_path
        ):
            client.start("127.0.0.1", "IPERF_ARGS", "TAG")
        mock_open.assert_called_with(file_path, "wb")
        mock_open().__enter__().write.assert_called_with(b"iperf test output")


class IPerfClientOverAdbTest(unittest.TestCase):
    """Test mobly_controller.iperf.iperf_client.IPerfClientOverAdb."""

    @mock.patch("builtins.open")
    def test_start_writes_output_to_full_file_path(
        self, mock_open: mock.Mock
    ) -> None:
        mock_adb = mock.Mock()
        mock_adb.adb.shell.return_value = "output"
        client = IPerfClientOverAdb(mock_adb)
        file_path = "/path/to/foo"

        with mock.patch.object(
            client, "_get_full_file_path", return_value=file_path
        ):
            client.start("127.0.0.1", "IPERF_ARGS", "TAG")

        mock_open.assert_called_with(file_path, "w")
        mock_open().__enter__().write.assert_called_with("output")


if __name__ == "__main__":
    unittest.main()
