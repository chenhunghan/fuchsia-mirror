#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for rbe_diagnose."""

import contextlib
import io
import shutil
import socket
import ssl
import subprocess
import time
import unittest
from unittest import mock

from build.auth import auth_env
from build.rbe import rbe_diagnose


class MeasureLatencyTests(unittest.TestCase):
    @mock.patch.object(time, "perf_counter")
    def test_latency_measured(self, mock_perf: mock.Mock) -> None:
        mock_perf.side_effect = [1.0, 3.5]
        with rbe_diagnose._measure_latency() as tracker:
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
        with contextlib.redirect_stdout(output):
            rbe_diagnose._print_status(rbe_diagnose.Status.OK, "Hello")
        self.assertIn("Hello", output.getvalue())
        self.assertNotIn("in", output.getvalue())

    def test_print_status_with_duration(self) -> None:
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            rbe_diagnose._print_status(rbe_diagnose.Status.OK, "Hello", 1.25)
        self.assertIn("Hello", output.getvalue())
        self.assertIn("in 1.250s", output.getvalue())


class DiagnoseDnsTests(unittest.TestCase):
    @mock.patch.object(socket, "getaddrinfo")
    def test_dns_success(self, mock_dns: mock.Mock) -> None:
        mock_dns.return_value = [
            (socket.AF_INET, socket.SOCK_STREAM, 6, "", ("1.2.3.4", 0))
        ]
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            result = rbe_diagnose._diagnose_dns("example.com")
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
        with contextlib.redirect_stdout(output):
            result = rbe_diagnose._diagnose_dns("example.com")
        self.assertIsNone(result)
        self.assertIn("DNS resolution failed", output.getvalue())


class DiagnoseTcpTests(unittest.TestCase):
    @mock.patch.object(socket, "socket")
    def test_tcp_success(self, mock_sock: mock.Mock) -> None:
        mock_s = mock_sock.return_value
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            sock = rbe_diagnose._diagnose_tcp("1.2.3.4", 443, socket.AF_INET)
        self.assertEqual(sock, mock_s)
        self.assertIn("TCP connection established", output.getvalue())

    @mock.patch.object(socket, "socket")
    def test_tcp_timeout(self, mock_sock: mock.Mock) -> None:
        mock_s = mock_sock.return_value
        mock_s.connect.side_effect = socket.timeout("Timed out")
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            sock = rbe_diagnose._diagnose_tcp("1.2.3.4", 443, socket.AF_INET)
        self.assertIsNone(sock)
        self.assertIn("TCP connection failed to 1.2.3.4", output.getvalue())


class DiagnoseTlsTests(unittest.TestCase):
    @mock.patch.object(ssl, "create_default_context")
    def test_tls_success(self, mock_ssl: mock.Mock) -> None:
        mock_s = mock.Mock()
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            success = rbe_diagnose._diagnose_tls(mock_s, "example.com")
        self.assertTrue(success)
        self.assertIn("TLS handshake successful", output.getvalue())

    @mock.patch.object(ssl, "create_default_context")
    def test_tls_failed(self, mock_ssl: mock.Mock) -> None:
        mock_ctx = mock_ssl.return_value
        mock_ctx.wrap_socket.side_effect = ssl.SSLError("SSL failed")
        mock_s = mock.Mock()
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            success = rbe_diagnose._diagnose_tls(mock_s, "example.com")
        self.assertFalse(success)
        self.assertIn("TLS handshake failed", output.getvalue())


class CheckGcloudAuthTests(unittest.TestCase):
    @mock.patch.object(shutil, "which")
    def test_gcloud_missing(self, mock_which: mock.Mock) -> None:
        mock_which.return_value = None
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            rbe_diagnose._check_gcloud_auth()
        self.assertIn("gcloud CLI not found on PATH", output.getvalue())

    @mock.patch.object(shutil, "which")
    @mock.patch.object(subprocess, "run")
    def test_gcloud_success(
        self, mock_run: mock.Mock, mock_which: mock.Mock
    ) -> None:
        mock_which.return_value = "/path/to/gcloud"
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            rbe_diagnose._check_gcloud_auth()
        self.assertIn("gcloud credential retrieval", output.getvalue())


class CheckLoasCertTests(unittest.TestCase):
    @mock.patch.object(rbe_diagnose, "shutil", autospec=True)
    def test_gcertstatus_missing(self, mock_shutil: mock.Mock) -> None:
        mock_shutil.which.return_value = None
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            valid = rbe_diagnose._check_loas_cert()
        self.assertFalse(valid)
        self.assertEqual(output.getvalue(), "")

    @mock.patch.object(rbe_diagnose, "shutil", autospec=True)
    @mock.patch.object(rbe_diagnose, "subprocess", autospec=True)
    def test_gcertstatus_valid(
        self, mock_subprocess: mock.Mock, mock_shutil: mock.Mock
    ) -> None:
        mock_shutil.which.return_value = "/path/to/gcertstatus"
        mock_subprocess.run.return_value = subprocess.CompletedProcess(
            args=(),
            returncode=0,
        )
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            valid = rbe_diagnose._check_loas_cert()
        self.assertTrue(valid)
        self.assertIn(
            "LOAS certificate valid and active",
            output.getvalue(),
        )

    @mock.patch.object(rbe_diagnose, "shutil", autospec=True)
    @mock.patch.object(rbe_diagnose, "subprocess", autospec=True)
    def test_gcertstatus_expired(
        self, mock_subprocess: mock.Mock, mock_shutil: mock.Mock
    ) -> None:
        mock_shutil.which.return_value = "/path/to/gcertstatus"
        mock_subprocess.CalledProcessError = subprocess.CalledProcessError
        mock_subprocess.TimeoutExpired = subprocess.TimeoutExpired
        mock_subprocess.run.side_effect = subprocess.CalledProcessError(
            1, "gcertstatus"
        )
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            valid = rbe_diagnose._check_loas_cert()
        self.assertFalse(valid)
        self.assertIn(
            "LOAS certificate is expired, invalid, or missing",
            output.getvalue(),
        )


class DiagnoseCredentialsTests(unittest.TestCase):
    @mock.patch.object(rbe_diagnose, "_check_bazel_cred_helper", autospec=True)
    @mock.patch.object(rbe_diagnose, "_check_gcloud_auth", autospec=True)
    @mock.patch.object(
        rbe_diagnose,
        "_check_loas_cert",
        autospec=True,
        return_value=True,
    )
    @mock.patch.object(
        auth_env, "resolve_build_auth_type", autospec=True, return_value="loas"
    )
    def test_loas_skips_gcloud_auth(
        self,
        _mock_resolve: mock.Mock,
        mock_loas: mock.Mock,
        mock_gcloud: mock.Mock,
        mock_helper: mock.Mock,
    ) -> None:
        rbe_diagnose._diagnose_credentials(env={})
        mock_loas.assert_called_once()
        mock_gcloud.assert_not_called()
        mock_helper.assert_called_once()

    @mock.patch.object(rbe_diagnose, "_check_bazel_cred_helper", autospec=True)
    @mock.patch.object(rbe_diagnose, "_check_gcloud_auth", autospec=True)
    @mock.patch.object(
        rbe_diagnose, "_check_loas_cert", autospec=True, return_value=False
    )
    @mock.patch.object(
        auth_env, "resolve_build_auth_type", autospec=True, return_value="oauth"
    )
    @mock.patch.object(rbe_diagnose, "shutil", autospec=True)
    def test_oauth_runs_gcloud_auth_and_skips_cred_helper(
        self,
        mock_shutil: mock.Mock,
        _mock_resolve: mock.Mock,
        mock_loas: mock.Mock,
        mock_gcloud: mock.Mock,
        mock_helper: mock.Mock,
    ) -> None:
        mock_shutil.which.return_value = None
        rbe_diagnose._diagnose_credentials(env={})
        mock_loas.assert_called_once()
        mock_gcloud.assert_called_once()
        mock_helper.assert_not_called()

    @mock.patch.object(rbe_diagnose, "_check_bazel_cred_helper", autospec=True)
    @mock.patch.object(rbe_diagnose, "_check_gcloud_auth", autospec=True)
    @mock.patch.object(
        rbe_diagnose, "_check_loas_cert", autospec=True, return_value=False
    )
    @mock.patch.object(
        auth_env, "cached_auth_type", autospec=True, return_value=None
    )
    @mock.patch.object(auth_env, "resolve_build_auth_type", autospec=True)
    @mock.patch.object(rbe_diagnose, "shutil", autospec=True)
    def test_expired_loas_corp_skips_resolve_and_gcloud(
        self,
        mock_shutil: mock.Mock,
        mock_resolve: mock.Mock,
        _mock_cached: mock.Mock,
        mock_loas: mock.Mock,
        mock_gcloud: mock.Mock,
        mock_helper: mock.Mock,
    ) -> None:
        mock_shutil.which.return_value = "/path/to/gcertstatus"
        rbe_diagnose._diagnose_credentials(env={})
        mock_loas.assert_called_once()
        mock_resolve.assert_not_called()
        mock_gcloud.assert_not_called()
        mock_helper.assert_called_once()


class MainTests(unittest.TestCase):
    @mock.patch.object(rbe_diagnose, "_diagnose_credentials", autospec=True)
    @mock.patch.object(
        rbe_diagnose, "_diagnose_endpoints", autospec=True, return_value=True
    )
    def test_main_passed(
        self, mock_endpoints: mock.Mock, mock_creds: mock.Mock
    ) -> None:
        output = io.StringIO()
        env = {"FOO": "bar"}
        with contextlib.redirect_stdout(output):
            rc = rbe_diagnose.main([], env=env)
        self.assertEqual(rc, 0)
        mock_endpoints.assert_called_once_with()
        mock_creds.assert_called_once_with(env=env)
        self.assertIn("PASSED", output.getvalue())

    @mock.patch.object(rbe_diagnose, "_diagnose_credentials", autospec=True)
    @mock.patch.object(
        rbe_diagnose, "_diagnose_endpoints", autospec=True, return_value=False
    )
    def test_main_failed_endpoints(
        self, mock_endpoints: mock.Mock, mock_creds: mock.Mock
    ) -> None:
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            rc = rbe_diagnose.main([], env={})
        self.assertEqual(rc, 0)
        mock_endpoints.assert_called_once_with()
        mock_creds.assert_called_once_with(env={})
        self.assertIn("FAILED", output.getvalue())


if __name__ == "__main__":
    unittest.main()
