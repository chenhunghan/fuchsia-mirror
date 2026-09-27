#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for rbe_diagnose."""

import io
import shutil
import socket
import ssl
import subprocess
import sys
import time
import unittest
from unittest import mock

import rbe_diagnose


class MeasureLatencyTests(unittest.TestCase):
    @mock.patch.object(time, "perf_counter")
    def test_latency_measured(self, mock_perf: mock.Mock) -> None:
        mock_perf.side_effect = [1.0, 3.5]
        with rbe_diagnose.measure_latency() as tracker:
            pass
        self.assertAlmostEqual(tracker.duration, 2.5)


class StatusEnumTests(unittest.TestCase):
    @mock.patch.object(rbe_diagnose, "IS_TTY", True)
    def test_status_str_ok(self) -> None:
        s = str(rbe_diagnose.Status.OK)
        self.assertIn("OK", s)
        self.assertIn(rbe_diagnose.GREEN, s)

    @mock.patch.object(rbe_diagnose, "IS_TTY", True)
    def test_status_str_warn(self) -> None:
        s = str(rbe_diagnose.Status.WARN)
        self.assertIn("WARN", s)
        self.assertIn(rbe_diagnose.YELLOW, s)

    @mock.patch.object(rbe_diagnose, "IS_TTY", True)
    def test_status_str_fail(self) -> None:
        s = str(rbe_diagnose.Status.FAIL)
        self.assertIn("FAIL", s)
        self.assertIn(rbe_diagnose.RED, s)

    @mock.patch.object(rbe_diagnose, "IS_TTY", False)
    def test_status_str_no_color(self) -> None:
        s = str(rbe_diagnose.Status.OK)
        self.assertEqual("[OK]", s)


class PrintStatusTests(unittest.TestCase):
    def test_print_status_no_duration(self) -> None:
        output = io.StringIO()
        with mock.patch.object(sys, "stdout", output):
            rbe_diagnose.print_status(rbe_diagnose.Status.OK, "Hello")
        self.assertIn("Hello", output.getvalue())
        self.assertNotIn("in", output.getvalue())

    def test_print_status_with_duration(self) -> None:
        output = io.StringIO()
        with mock.patch.object(sys, "stdout", output):
            rbe_diagnose.print_status(rbe_diagnose.Status.OK, "Hello", 1.25)
        self.assertIn("Hello", output.getvalue())
        self.assertIn("in 1.250s", output.getvalue())


class DiagnoseDnsTests(unittest.TestCase):
    @mock.patch.object(socket, "getaddrinfo")
    def test_dns_success(self, mock_dns: mock.Mock) -> None:
        mock_dns.return_value = [
            (socket.AF_INET, socket.SOCK_STREAM, 6, "", ("1.2.3.4", 0))
        ]
        output = io.StringIO()
        with mock.patch.object(sys, "stdout", output):
            result = rbe_diagnose.diagnose_dns("example.com")
        self.assertIsNotNone(result)
        assert result is not None
        self.assertIsInstance(result, rbe_diagnose.DnsResult)
        self.assertEqual(result.ip, "1.2.3.4")
        self.assertEqual(result.family, socket.AF_INET)
        self.assertIn("DNS resolution", output.getvalue())

    @mock.patch.object(socket, "getaddrinfo")
    def test_dns_failed(self, mock_dns: mock.Mock) -> None:
        mock_dns.side_effect = socket.gaierror("DNS failed")
        output = io.StringIO()
        with mock.patch.object(sys, "stdout", output):
            result = rbe_diagnose.diagnose_dns("example.com")
        self.assertIsNone(result)
        self.assertIn("DNS resolution failed", output.getvalue())


class DiagnoseTcpTests(unittest.TestCase):
    @mock.patch.object(socket, "socket")
    def test_tcp_success(self, mock_sock: mock.Mock) -> None:
        mock_s = mock_sock.return_value
        output = io.StringIO()
        with mock.patch.object(sys, "stdout", output):
            sock = rbe_diagnose.diagnose_tcp("1.2.3.4", 443, socket.AF_INET)
        self.assertEqual(sock, mock_s)
        self.assertIn("TCP connection established", output.getvalue())

    @mock.patch.object(socket, "socket")
    def test_tcp_timeout(self, mock_sock: mock.Mock) -> None:
        mock_s = mock_sock.return_value
        mock_s.connect.side_effect = socket.timeout("Timed out")
        output = io.StringIO()
        with mock.patch.object(sys, "stdout", output):
            sock = rbe_diagnose.diagnose_tcp("1.2.3.4", 443, socket.AF_INET)
        self.assertIsNone(sock)
        self.assertIn("TCP connection failed to 1.2.3.4", output.getvalue())


class DiagnoseTlsTests(unittest.TestCase):
    @mock.patch.object(ssl, "create_default_context")
    def test_tls_success(self, mock_ssl: mock.Mock) -> None:
        mock_s = mock.Mock()
        output = io.StringIO()
        with mock.patch.object(sys, "stdout", output):
            success = rbe_diagnose.diagnose_tls(mock_s, "example.com")
        self.assertTrue(success)
        self.assertIn("TLS handshake successful", output.getvalue())

    @mock.patch.object(ssl, "create_default_context")
    def test_tls_failed(self, mock_ssl: mock.Mock) -> None:
        mock_ctx = mock_ssl.return_value
        mock_ctx.wrap_socket.side_effect = ssl.SSLError("SSL failed")
        mock_s = mock.Mock()
        output = io.StringIO()
        with mock.patch.object(sys, "stdout", output):
            success = rbe_diagnose.diagnose_tls(mock_s, "example.com")
        self.assertFalse(success)
        self.assertIn("TLS handshake failed", output.getvalue())


class CheckGcloudAuthTests(unittest.TestCase):
    @mock.patch.object(shutil, "which")
    def test_gcloud_missing(self, mock_which: mock.Mock) -> None:
        mock_which.return_value = None
        output = io.StringIO()
        with mock.patch.object(sys, "stdout", output):
            rbe_diagnose.check_gcloud_auth()
        self.assertIn("gcloud CLI not found on PATH", output.getvalue())

    @mock.patch.object(shutil, "which")
    @mock.patch.object(subprocess, "run")
    def test_gcloud_success(
        self, mock_run: mock.Mock, mock_which: mock.Mock
    ) -> None:
        mock_which.return_value = "/path/to/gcloud"
        output = io.StringIO()
        with mock.patch.object(sys, "stdout", output):
            rbe_diagnose.check_gcloud_auth()
        self.assertIn("gcloud credential retrieval", output.getvalue())


if __name__ == "__main__":
    unittest.main()
