# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Measures USB 2.0 bulk throughput of the Fuchsia USB device stack.

The device under test runs the usb-zero-function driver, and the Linux host
it is plugged into generates the traffic over a real USB cable using
userspace usbdevfs bulk transfers (without loading the Linux usbtest kernel
driver):

- Bulk OUT (host to device): 64 KiB bulk OUT transfers on Configuration 1
  (source/sink).
- Bulk IN (device to host): 64 KiB bulk IN transfers on Configuration 1
  (source/sink).
- Bidirectional: 64 KiB bulk loopback transfers on Configuration 2
  (loopback).

Each case publishes Rate and WallTime fuchsiaperf metrics. As with other
Fuchsia performance tests, the test does not fail on the measured values,
only when a measurement cannot be completed or the loopback data is
corrupted.

For reference only, the USB M5 exit criteria (see b/523413021) are more
than 30 MB/s each for bulk IN and bulk OUT over a 1 GB transfer (see
zero_function_throughput_large_test), and more than 35 MB/s combined for
simultaneous bulk IN and OUT transfers. The test reports the measured rates
but does not check them against these criteria.
"""

import logging
import math
import pathlib
import time

import perf_publish.publish as publish
import test_data
import zero_function
from mobly import asserts, test_runner
from reporting import metrics
from testusb import TestParams, TestResult, TestStatus, USBTestIoctlBackend

_LOGGER: logging.Logger = logging.getLogger(__name__)

_DEFAULT_TOTAL_TRANSFER_BYTES: int = 52_428_800  # 50 MiB
# Matches the 64 KiB VMOs that usb-zero-function queues its requests in.
_TRANSFER_LENGTH: int = 65536

_BULK_OUT_METRIC_SUITE: str = "fuchsia.usb.zero.bulk_out"
_BULK_IN_METRIC_SUITE: str = "fuchsia.usb.zero.bulk_in"
_BIDIRECTIONAL_METRIC_SUITE: str = "fuchsia.usb.zero.bidirectional"


class ZeroFunctionThroughputTest(zero_function.ZeroFunctionBaseTest):
    """Measures sustained bulk DMA throughput across USB 2.0 endpoints using usb-zero-function."""

    # All three benchmarks drive bulk endpoints directly from userspace via
    # usbdevfs, so the Linux usbtest kernel driver is never needed.
    LOAD_USBTEST_DRIVER: bool = False

    async def setup_class(self) -> None:
        """Initializes USB Zero Function mode and reads benchmark parameters."""
        await super().setup_class()

        self._total_transfer_bytes = int(
            self.user_params.get(
                "total_transfer_bytes", _DEFAULT_TOTAL_TRANSFER_BYTES
            )
        )

        _LOGGER.info(
            "Configured USB Zero Function throughput benchmark: "
            "total_transfer_bytes=%d (%.1f MiB)",
            self._total_transfer_bytes,
            self._total_transfer_bytes / (1024 * 1024),
        )

    def _run_bulk(
        self,
        backend: USBTestIoctlBackend,
        params: TestParams,
        config_val: int,
        use_out: bool,
        use_in: bool,
        metric_suite: str,
    ) -> TestResult:
        """Runs synchronous bulk transfers via USBDEVFS_BULK."""
        if use_out and use_in:
            return backend.run_bulk_loopback(params, test_name=metric_suite)
        asserts.assert_true(
            backend.set_configuration(config_val),
            f"Failed to set Configuration {config_val}",
        )
        ep_out, ep_in, claim_ifnum = backend._find_endpoints(
            config_val=config_val
        )
        ep = ep_out if use_out else ep_in
        asserts.assert_is_not_none(ep, "Bulk endpoint not found")
        assert ep is not None
        dev = zero_function.LoopbackDevice(
            backend, ep_out or 0, ep_in or 0, 512
        )
        payload: bytes | int = (
            bytes(params.length) if use_out else params.length
        )
        with backend.claimed_interface(
            claim_ifnum if claim_ifnum is not None else backend.ifnum
        ):
            start = time.perf_counter()
            for i in range(params.iterations):
                data = dev.bulk(ep, payload, timeout=params.timeout_ms)
                # LoopbackDevice.bulk() already fails short OUT writes, but
                # returns short IN reads, which would inflate the rate.
                if use_in and len(data) != params.length:
                    asserts.fail(
                        f"Bulk IN transfer {i} was short: requested "
                        f"{params.length} bytes, received {len(data)} bytes"
                    )
            elapsed = time.perf_counter() - start
        return TestResult(
            test_id=0,
            test_name=metric_suite,
            status=TestStatus.PASS,
            duration_secs=elapsed,
            iterations_completed=params.iterations,
            bytes_transferred=params.iterations * params.length,
        )

    def _run_and_publish(
        self,
        metric_suite: str,
        config_val: int,
        use_out: bool,
        use_in: bool,
    ) -> None:
        """Runs a bulk transfer benchmark and publishes fuchsiaperf metrics."""
        directions = int(use_out) + int(use_in)
        bytes_per_iteration = _TRANSFER_LENGTH * directions
        iterations = max(
            1, math.ceil(self._total_transfer_bytes / bytes_per_iteration)
        )
        params = TestParams(
            iterations=iterations,
            length=_TRANSFER_LENGTH,
        )

        with USBTestIoctlBackend(
            device_path=self.require_dev_node()
        ) as backend:
            assert isinstance(backend, USBTestIoctlBackend)
            result = self._run_bulk(
                backend=backend,
                params=params,
                config_val=config_val,
                use_out=use_out,
                use_in=use_in,
                metric_suite=metric_suite,
            )

        asserts.assert_equal(
            result.status,
            TestStatus.PASS,
            f"{metric_suite} failed: {result.error_message or 'no details'}",
        )
        asserts.assert_greater(result.duration_secs, 0.0)
        asserts.assert_greater(result.bytes_transferred, 0)

        rate = result.bytes_transferred / result.duration_secs
        wall_time_ms = result.duration_secs * 1000.0
        test_case_results = [
            metrics.TestCaseResult(
                "Rate",
                metrics.Unit.bytesPerSecond,
                [rate],
                "Data transfer speed",
            ),
            metrics.TestCaseResult(
                "WallTime",
                metrics.Unit.milliseconds,
                [wall_time_ms],
                f"Time taken to transfer {result.bytes_transferred} bytes",
            ),
        ]

        results_file = (
            pathlib.Path(self.test_case_path)
            / f"{metric_suite}.fuchsiaperf.json"
        )
        metrics.TestCaseResult.write_fuchsiaperf_json(
            results=test_case_results,
            test_suite=metric_suite,
            output_path=results_file,
        )
        publish.publish_fuchsiaperf(
            [results_file],
            f"{metric_suite}.txt",
            test_data_module=test_data,
        )

        _LOGGER.info(
            "%s completed: %d bytes in %.3fs (%.2f MB/s, %.2f MiB/s)",
            metric_suite,
            result.bytes_transferred,
            result.duration_secs,
            rate / 1_000_000,
            rate / (1024 * 1024),
        )

    def test_bulk_out_throughput(self) -> None:
        """Measures sustained Bulk OUT (Host -> Device) DMA throughput (M5 E1.2)."""
        self._run_and_publish(
            metric_suite=_BULK_OUT_METRIC_SUITE,
            config_val=1,
            use_out=True,
            use_in=False,
        )

    def test_bulk_in_throughput(self) -> None:
        """Measures sustained Bulk IN (Device -> Host) DMA throughput (M5 E1.1)."""
        self._run_and_publish(
            metric_suite=_BULK_IN_METRIC_SUITE,
            config_val=1,
            use_out=False,
            use_in=True,
        )

    def test_bidirectional_throughput(self) -> None:
        """Measures combined Bulk IN + OUT loopback DMA throughput (M5 E1.3)."""
        with self.open_loopback_device() as dev:
            dev.configure_scatter_gather(
                [(0, _TRANSFER_LENGTH)],
                [(0, _TRANSFER_LENGTH)],
            )
        try:
            self._run_and_publish(
                metric_suite=_BIDIRECTIONAL_METRIC_SUITE,
                config_val=2,
                use_out=True,
                use_in=True,
            )
        finally:
            # Log instead of raising: if the device wedged, the reset fails
            # too and would otherwise hide the original failure.
            try:
                with self.open_loopback_device() as dev:
                    dev.reset_scatter_gather()
            except Exception as e:
                _LOGGER.warning("Failed to reset scatter-gather: %s", e)


if __name__ == "__main__":
    test_runner.main()
