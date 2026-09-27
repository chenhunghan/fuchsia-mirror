#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""RBE and BES connection diagnostic tool."""

import contextlib
import enum
import shutil
import socket
import ssl
import subprocess
import sys
import time
from pathlib import Path
from typing import Generator, NamedTuple, Optional

# ANSI colors for status reports
GREEN = "\033[92m"
YELLOW = "\033[93m"
RED = "\033[91m"
RESET = "\033[0m"
BOLD = "\033[1m"

# Warning Latency Thresholds (in seconds)
WARN_THRESHOLD_DNS = 0.5
WARN_THRESHOLD_TCP = 0.3
WARN_THRESHOLD_TLS = 0.5
WARN_THRESHOLD_GCLOUD = 1.5
WARN_THRESHOLD_CRED_HELPER = 1.0

# Timeout Durations (in seconds)
TIMEOUT_ENDPOINT = 5.0
TIMEOUT_GCLOUD = 5.0
TIMEOUT_CRED_HELPER = 5.0
TIMEOUT_LOAS = 3.0

# RBE & BES Endpoints to check
ENDPOINTS = [
    "remotebuildexecution.googleapis.com",
    "buildeventservice.googleapis.com",
    "resultstore.googleapis.com",
    "storage.googleapis.com",
]

# Check once at load time if stdout is a TTY for colorization
IS_TTY = sys.stdout.isatty()


def colorize(text: str, color_code: str) -> str:
    """Wraps text in ANSI escape code with an automatic reset if stdout is a TTY."""
    if not IS_TTY:
        return text
    return f"{color_code}{text}{RESET}"


class Status(enum.Enum):
    """Diagnostic check outcomes."""

    OK = "OK"
    WARN = "WARN"
    FAIL = "FAIL"

    def __str__(self) -> str:
        if self == Status.OK:
            return colorize("[OK]", GREEN)
        elif self == Status.WARN:
            return colorize("[WARN]", YELLOW)
        elif self == Status.FAIL:
            return colorize("[FAIL]", RED)
        return f"[{self.value}]"


class DnsResult(NamedTuple):
    """Holds the resolved IP and its address family."""

    ip: str
    family: int


class LatencyTracker:
    """A simple container to hold the duration of a timed block."""

    def __init__(self) -> None:
        self.duration: float = 0.0


@contextlib.contextmanager
def measure_latency() -> Generator[LatencyTracker, None, None]:
    tracker = LatencyTracker()
    start = time.perf_counter()
    try:
        yield tracker
    finally:
        tracker.duration = time.perf_counter() - start


def print_status(
    status: Status, message: str, duration: Optional[float] = None
) -> None:
    dur_str = f" in {duration:.3f}s" if duration is not None else ""
    print(f"{status} {message}{dur_str}")


def diagnose_dns(host: str) -> Optional[DnsResult]:
    """Diagnose DNS resolution for the host."""
    try:
        with measure_latency() as dns:
            # Use getaddrinfo as it is modern and protocol-agnostic (IPv4 & IPv6)
            addr_info = socket.getaddrinfo(host, None)
            family = addr_info[0][0]
            ip = addr_info[0][4][0]
        if dns.duration > WARN_THRESHOLD_DNS:
            print_status(
                Status.WARN,
                f"DNS resolution for {host} was slow: {ip}",
                dns.duration,
            )
        else:
            print_status(Status.OK, f"DNS resolution: {ip}", dns.duration)
        return DnsResult(ip=ip, family=family)
    except (socket.gaierror, OSError) as e:
        print_status(Status.FAIL, f"DNS resolution failed for {host}: {e}")
        return None


def diagnose_tcp(
    ip: str, port: int, family: int, timeout: float = TIMEOUT_ENDPOINT
) -> Optional[socket.socket]:
    """Diagnose raw TCP socket connection to the IP and port."""
    sock: Optional[socket.socket] = None
    try:
        sock = socket.socket(family, socket.SOCK_STREAM)
        sock.settimeout(timeout)
        with measure_latency() as tcp:
            sock.connect((ip, port))
        if tcp.duration > WARN_THRESHOLD_TCP:
            print_status(
                Status.WARN,
                f"TCP connection to {ip} was slow (high physical/VPN latency)",
                tcp.duration,
            )
        else:
            print_status(
                Status.OK, f"TCP connection established to {ip}", tcp.duration
            )
        return sock
    except (socket.timeout, OSError) as e:
        print_status(Status.FAIL, f"TCP connection failed to {ip}: {e}")
        if sock is not None:
            sock.close()
        return None


def diagnose_tls(sock: socket.socket, host: str) -> bool:
    """Diagnose TLS handshake overhead on the connected TCP socket."""
    try:
        context = ssl.create_default_context()
        with measure_latency() as tls:
            ssl_sock = context.wrap_socket(sock, server_hostname=host)
        if tls.duration > WARN_THRESHOLD_TLS:
            print_status(
                Status.WARN,
                "TLS handshake was slow (possible proxy or severe physical roundtrip overhead)",
                tls.duration,
            )
        else:
            print_status(Status.OK, "TLS handshake successful", tls.duration)
        ssl_sock.close()
        return True
    except (ssl.SSLError, socket.timeout, OSError) as e:
        print_status(Status.FAIL, f"TLS handshake failed: {e}")
        sock.close()
        return False


def diagnose_endpoint(
    host: str, port: int = 443, timeout: float = TIMEOUT_ENDPOINT
) -> bool:
    print(f"\nChecking connection to {colorize(f'{host}:{port}', BOLD)}...")

    dns_result = diagnose_dns(host)
    if not dns_result:
        return False

    sock = diagnose_tcp(dns_result.ip, port, dns_result.family, timeout)
    if not sock:
        return False

    return diagnose_tls(sock, host)


def check_gcloud_auth() -> None:
    print(f"\nChecking local gcloud credential latency...")
    # Check if gcloud is on PATH
    gcloud_path = shutil.which("gcloud")
    if not gcloud_path:
        print_status(
            Status.WARN,
            "gcloud CLI not found on PATH. RBE/BES might use alternative authentication.",
        )
        return

    # Measure latency of print-access-token
    try:
        # Avoid showing standard error outputs or stdout tokens
        with measure_latency() as token_retrieval:
            subprocess.run(
                [
                    gcloud_path,
                    "auth",
                    "application-default",
                    "print-access-token",
                ],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                check=True,
                timeout=TIMEOUT_GCLOUD,
            )
        if token_retrieval.duration > WARN_THRESHOLD_GCLOUD:
            print_status(
                Status.WARN,
                "gcloud token retrieval is slow",
                token_retrieval.duration,
            )
        else:
            print_status(
                Status.OK,
                "gcloud credential retrieval (ADC)",
                token_retrieval.duration,
            )
    except subprocess.TimeoutExpired:
        print_status(
            Status.FAIL,
            f"gcloud token retrieval timed out (>{TIMEOUT_GCLOUD}s)",
        )
    except (subprocess.CalledProcessError, OSError) as e:
        print_status(Status.FAIL, f"gcloud token retrieval failed: {e}")


def check_bazel_cred_helper() -> None:
    cred_helper = Path(
        "/google/src/head/depot/google3/devtools/blaze/bazel/credhelper/credhelper"
    )
    # We do NOT call cred_helper.exists() because it can block if CitC is hung.
    # Instead, we execute the helper directly with a timeout. If it is not found
    # (e.g. non-corp environment), we silently return.
    try:
        # Running the helper without arguments usually prints help/errors out quickly
        with measure_latency() as helper_run:
            subprocess.run(
                [str(cred_helper)],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                timeout=TIMEOUT_CRED_HELPER,
            )
    except FileNotFoundError:
        # Silently return since we're in a non-corp environment or CitC is not used
        return
    except subprocess.TimeoutExpired:
        print(f"\nChecking Bazel Credential Helper latency...")
        print_status(
            Status.FAIL,
            f"Access to Bazel credential helper on CitC/SrcFS timed out (>{TIMEOUT_CRED_HELPER}s). CitC/SrcFS mount might be dead.",
        )
        return
    except OSError as e:
        print(f"\nChecking Bazel Credential Helper latency...")
        print_status(
            Status.FAIL, f"Failed to execute Bazel credential helper: {e}"
        )
        return

    # If it ran successfully, print the health metrics
    print(f"\nChecking Bazel Credential Helper latency...")
    if helper_run.duration > WARN_THRESHOLD_CRED_HELPER:
        print_status(
            Status.WARN,
            f"Access to Bazel credential helper on CitC/SrcFS is slow ({cred_helper})",
            helper_run.duration,
        )
        print(
            "  This can cause Bazel's Capabilities check or credential retrieval to timeout."
        )
        print(
            "  We recommend checking SrcFS/CitC mount health or warming the cache by running:"
        )
        print(f"    ls -l {cred_helper}")
    else:
        print_status(
            Status.OK,
            "Bazel credential helper startup time",
            helper_run.duration,
        )


def check_loas_cert() -> None:
    # Skip LOAS check entirely if loas_check is not on PATH (non-corp or non-FTE)
    loas_path = shutil.which("loas_check")
    if not loas_path:
        return

    print(f"\nChecking LOAS certificate latency...")
    try:
        # Check if loas_check tool is present
        with measure_latency() as loas_check:
            subprocess.run(
                [loas_path],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                check=True,
                timeout=TIMEOUT_LOAS,
            )
        print_status(
            Status.OK, "LOAS certificate valid and active", loas_check.duration
        )
    except subprocess.CalledProcessError:
        # loas_check exits with non-zero if LOAS is expired or missing
        print_status(
            Status.WARN, "LOAS certificate is expired, invalid, or missing."
        )
    except subprocess.TimeoutExpired:
        print_status(Status.FAIL, f"LOAS check timed out (>{TIMEOUT_LOAS}s)")


def diagnose_endpoints() -> bool:
    """Diagnose connection latency to primary endpoints."""
    # list-comprehension to force-evaluate all without short-circuit
    return all([diagnose_endpoint(endpoint) for endpoint in ENDPOINTS])


def diagnose_credentials() -> None:
    """Diagnose credential retrieval latency."""
    check_loas_cert()
    check_gcloud_auth()
    check_bazel_cred_helper()


def main() -> None:
    print(f"{colorize('=== Fuchsia RBE & BES Diagnostic Tool ===', BOLD)}")

    endpoints_success = diagnose_endpoints()
    diagnose_credentials()

    print(f"\n{colorize('=== Diagnostic Completed ===', BOLD)}")
    if endpoints_success:
        print(
            f"Connection tests to all RBE/BES endpoints {colorize('PASSED', GREEN)}."
        )
    else:
        print(
            f"One or more connection tests {colorize('FAILED', RED)}. Please check your network or VPN configuration."
        )


if __name__ == "__main__":
    main()
