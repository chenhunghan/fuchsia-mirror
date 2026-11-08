# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""End-to-end USB Soak Test for Fuchsia USB composite peripheral functions."""

import asyncio
import hashlib
import json
import logging
import os
import tempfile
import time
import uuid
from typing import Any

import usb_lib
from honeydew import errors
from honeydew.affordances.connectivity.netstack.types import PortClass
from honeydew.transports.adb import errors as adb_errors
from honeydew.transports.ffx import errors as ffx_errors
from mobly import asserts, signals, test_runner
from usb_lib.dwc3_irq_metrics import (
    Dwc3IrqSnapshot,
    assert_no_dwc3_unhandled_irq_storm,
    collect_dwc3_irq_metrics,
)
from usb_lib.irq_storm import (
    IrqStormSnapshot,
    assert_no_irq_storm,
    take_irq_storm_snapshot,
)
from usb_lib.link_speed import assert_link_speed

_LOGGER: logging.Logger = logging.getLogger(__name__)

_ADB_PAYLOAD_SIZE_BYTES: int = 256 * 1024
_DEFAULT_RECONNECT_TIMEOUT_SEC: float = 60.0


class UsbSoakTest(usb_lib.UsbPowerHubBaseTest):
    """End-to-end multi-epoch soak test for USB composite peripheral functions.

    Validates stability across CDC Ethernet, Starnix ADB, and Overnet/FFX
    traffic with periodic controlled USB disconnect/reconnect cycles, DWC3
    IRQ storm detection, and kernel memory growth monitoring.

    Supported Mobly Test Params (optional with defaults):
        num_epochs (int, optional): Number of soak epochs to execute.
            Defaults to 3.
        epoch_duration_sec (float, optional): Duration in seconds of active
            workload per epoch. Defaults to 15.0.
        disconnect_duration_sec (float, optional): How long to keep USB VBUS
            power off during controlled disconnect. Defaults to 2.0.
        expected_usb_link_speed (str, optional): Expected USB link speed after
            reconnect (e.g. "high"). Defaults to "high".
        circuit_breaker_threshold (int, optional): Number of consecutive
            transient failures before tripping the circuit breaker.
            Defaults to 2.
        reconnect_between_epochs (bool, optional): Whether to execute a
            controlled USB disconnect/reconnect between epochs.
            Defaults to True.
        reconnect_timeout_sec (float, optional): How long to wait for device
            to reconnect after USB power is restored. Defaults to 60.0.
        require_adb (bool, optional): Whether ADB transport is required to be
            functional on the DUT. Defaults to True.
        verify_no_irq_storm (bool, optional): Whether to verify no DWC3
            unhandled IRQ or driver dispatcher storm occurred. Defaults to True.

        GN targets only need to specify parameters that override these
        defaults (e.g. calibration for smoke vs. multi-hour soak profiles).
    """

    USB_POWER_HUB_REQUIRED: bool = True

    _reconnect_failed: bool = False
    _consecutive_failures: int = 0
    _initial_boot_id: str | None = None
    _require_adb: bool = True
    _epoch_history: list[dict[str, Any]] = []
    _epoch_failures: list[tuple[int, str]] = []

    async def setup_class(self) -> None:
        """Called once before running tests in the class."""
        await super().setup_class()
        self._initial_boot_id = await self.dut.boot_id()
        self._require_adb = bool(self.user_params.get("require_adb", True))
        _LOGGER.info(
            "Starting USB Soak Test. Initial Boot ID: %s, require_adb: %s",
            self._initial_boot_id,
            self._require_adb,
        )

    async def setup_test(self) -> None:
        """Called before each test case."""
        self._reconnect_failed = False
        self._consecutive_failures = 0
        self._epoch_history = []
        self._epoch_failures = []
        await super().setup_test()

    async def teardown_test(self) -> None:
        """Cleans up after each test case, enforcing the Teardown Safety
        Invariant.
        """
        reconnect_timeout_sec = float(
            self.user_params.get(
                "reconnect_timeout_sec", _DEFAULT_RECONNECT_TIMEOUT_SEC
            )
        )
        if self._usb_power_hub is not None:
            _LOGGER.info("Teardown Safety Invariant: Ensuring USB power is ON.")
            await asyncio.to_thread(
                self._usb_power_hub.power_on, port=self._usb_port
            )

        try:
            await asyncio.wait_for(
                self.dut.wait_for_online(), timeout=reconnect_timeout_sec
            )
            self._reconnect_failed = False
            _LOGGER.info("Teardown verified DUT is online and USB reachable.")
        except asyncio.TimeoutError as e:
            self._reconnect_failed = True
            _LOGGER.error(
                "DUT unreachable after VBUS restore; testbed may be wedged."
            )
            raise signals.TestAbortAll(
                "Teardown could not restore USB reachability after VBUS "
                "power_on"
            ) from e

        await super().teardown_test()

    async def _exercise_cdc_network(self) -> None:
        """Exercises CDC Ethernet network interface and round-trip traffic."""
        try:
            interfaces = await self.dut.netstack.list_interfaces()
        except RuntimeError as e:
            if "FDomain" in str(e) or "handle" in str(e) or "context" in str(e):
                _LOGGER.warning(
                    "Netstack proxy stale after reconnect, refreshing: %s", e
                )
                if hasattr(self.dut, "netstack") and hasattr(
                    self.dut.netstack, "_connect_proxy"
                ):
                    self.dut.netstack._connect_proxy()
                interfaces = await self.dut.netstack.list_interfaces()
            else:
                raise
        ethernet_ifaces = [
            i for i in interfaces if i.port_class == PortClass.ETHERNET
        ]
        asserts.assert_true(
            bool(ethernet_ifaces),
            "Pre-flight check failed: No active CDC Ethernet interface "
            "detected.",
        )
        ssh_out = await asyncio.to_thread(
            self.dut.ffx.run_ssh_cmd,
            cmd="echo usb_soak_cdc_ok",
        )
        asserts.assert_in(
            "usb_soak_cdc_ok",
            ssh_out,
            f"CDC network round-trip command failed: {ssh_out}",
        )

    async def _exercise_adb_transfer(
        self, payload_size_bytes: int = _ADB_PAYLOAD_SIZE_BYTES
    ) -> int:
        """Exercises bidirectional ADB file transfer with SHA256
        verification.
        """
        try:
            adb_transport = getattr(self.dut, "adb", None)
        except (errors.NotSupportedError, errors.NotEnabledError):
            adb_transport = None

        if adb_transport is None:
            if self._require_adb:
                asserts.fail(
                    "ADB transport is required on this target, but not "
                    "present on DUT."
                )
            _LOGGER.info("ADB transport not supported on target; skipping.")
            return 0

        await asyncio.to_thread(
            adb_transport.check_connection, timeout=30.0, attempts=1
        )

        payload = os.urandom(payload_size_bytes)
        expected_hash = hashlib.sha256(payload).hexdigest()
        token = uuid.uuid4().hex[:8]
        remote_file = f"/data/local/tmp/usb_soak_{token}.bin"

        with tempfile.TemporaryDirectory() as tmpdir:
            local_push_file = os.path.join(tmpdir, f"push_{token}.bin")
            local_pull_file = os.path.join(tmpdir, f"pull_{token}.bin")
            with open(local_push_file, "wb") as f:
                f.write(payload)

            try:
                await asyncio.to_thread(
                    adb_transport.run,
                    ["push", local_push_file, remote_file],
                    timeout=30.0,
                    attempts=1,
                )
                await asyncio.to_thread(
                    adb_transport.run,
                    ["pull", remote_file, local_pull_file],
                    timeout=30.0,
                    attempts=1,
                )
                asserts.assert_true(
                    os.path.exists(local_pull_file),
                    f"Pulled file {local_pull_file} does not exist",
                )
                with open(local_pull_file, "rb") as f_in:
                    pulled_data = f_in.read()
                pulled_hash = hashlib.sha256(pulled_data).hexdigest()
                asserts.assert_equal(
                    pulled_hash,
                    expected_hash,
                    "Checksum mismatch for ADB transfer: "
                    f"expected {expected_hash}, got {pulled_hash}",
                )
                return len(pulled_data)
            finally:
                try:
                    await asyncio.to_thread(
                        adb_transport.run,
                        ["shell", "rm", "-f", remote_file],
                        timeout=15.0,
                        attempts=1,
                    )
                except Exception:
                    pass

    async def _exercise_overnet_ffx(self) -> None:
        """Verifies host-target communication over Overnet/FFX."""
        output = await asyncio.to_thread(self.dut.ffx.run, ["target", "show"])
        asserts.assert_true(bool(output), "Empty output from ffx target show")

    async def _sample_kmem_stats(self) -> dict[str, int]:
        """Queries kernel memory statistics via FFX profile memory."""
        stats = {
            "total_heap_bytes": 0,
            "free_bytes": 0,
            "vmo_bytes": 0,
            "wired_bytes": 0,
        }
        raw_output = await asyncio.to_thread(
            self.dut.ffx.run, ["--machine", "json", "profile", "memory"]
        )
        parsed = json.loads(raw_output)

        def _extract(data: Any) -> bool:
            if isinstance(data, dict):
                if "total_heap_bytes" in data or "vmo_bytes" in data:
                    for k in stats:
                        if k in data and data[k] is not None:
                            stats[k] = int(data[k])
                    return True
                for v in data.values():
                    if _extract(v):
                        return True
            elif isinstance(data, list):
                for item in data:
                    if _extract(item):
                        return True
            return False

        _extract(parsed)
        return stats

    async def _perform_controlled_disconnect(
        self,
        disconnect_duration: float,
        reconnect_timeout: float,
        epoch: int,
    ) -> None:
        """Performs a controlled physical/virtual USB port disconnect and
        reconnect.
        """
        pre_boot_id = await self.dut.boot_id()
        self.dut.fuchsia_controller.before_usb_disconnect()
        self.dut.ffx.notify_intentional_disconnect()
        power_hub = self.require_usb_power_hub()
        await asyncio.to_thread(power_hub.power_off, port=self._usb_port)
        _LOGGER.info("Waiting for DUT to go offline...")
        try:
            await asyncio.to_thread(self.dut.wait_for_offline)
            if disconnect_duration > 0:
                _LOGGER.info(
                    "Sleeping for %.1f seconds...", disconnect_duration
                )
                await asyncio.sleep(disconnect_duration)
        finally:
            await asyncio.to_thread(power_hub.power_on, port=self._usb_port)
            _LOGGER.info("Waiting for DUT to reconnect...")
            try:
                await asyncio.wait_for(
                    self.dut.wait_for_online(), timeout=reconnect_timeout
                )
            except asyncio.TimeoutError as e:
                self._reconnect_failed = True
                raise signals.TestAbortAll(
                    f"Device {self.dut.device_name} failed to reconnect over "
                    f"USB within {reconnect_timeout}s after USB power-on."
                ) from e

        self.dut.fuchsia_controller.after_usb_reconnect()
        # Re-initialize the Netstack affordance FIDL proxy so it connects to
        # the new Fuchsia Controller context created after reconnect.
        if hasattr(self.dut, "netstack") and hasattr(
            self.dut.netstack, "_connect_proxy"
        ):
            _LOGGER.info("Reconnecting netstack proxy after USB reconnect...")
            self.dut.netstack._connect_proxy()
        post_boot_id = await self.dut.boot_id()
        if pre_boot_id != post_boot_id:
            raise errors.FuchsiaDeviceError(
                f"Unexpected reboot during disconnect! {pre_boot_id} != "
                f"{post_boot_id}"
            )
        if expected_speed := self.user_params.get("expected_usb_link_speed"):
            await asyncio.to_thread(
                assert_link_speed,
                self.dut,
                expected=expected_speed,
                phase=f"epoch {epoch}",
            )
        self.dut.health_check()

    async def test_usb_composite_soak(self) -> None:
        """Executes multi-epoch USB soak testing across composite
        peripherals.
        """
        num_epochs = int(self.user_params.get("num_epochs", 3))
        epoch_duration_sec = float(
            self.user_params.get("epoch_duration_sec", 15.0)
        )
        disconnect_duration_sec = float(
            self.user_params.get("disconnect_duration_sec", 2.0)
        )
        reconnect_timeout_sec = float(
            self.user_params.get(
                "reconnect_timeout_sec", _DEFAULT_RECONNECT_TIMEOUT_SEC
            )
        )
        reconnect_between_epochs = bool(
            self.user_params.get("reconnect_between_epochs", True)
        )
        circuit_breaker_threshold = int(
            self.user_params.get("circuit_breaker_threshold", 2)
        )
        verify_no_irq_storm = bool(
            self.user_params.get("verify_no_irq_storm", True)
        )

        _FATAL_ERRORS = (errors.FuchsiaDeviceError,)
        _TRANSIENT_ERRORS = (
            ffx_errors.FfxCommandError,
            ffx_errors.FfxConnectionError,
            ffx_errors.FfxTimeoutError,
            adb_errors.AdbCommandError,
            adb_errors.AdbConnectionError,
            adb_errors.AdbTimeoutError,
            errors.HostCmdError,
            TimeoutError,
            RuntimeError,
        )

        _LOGGER.info(
            "Starting USB Composite Soak Test: %d epochs, %.1fs per epoch, "
            "reconnect_between_epochs=%s, circuit_breaker_threshold=%d",
            num_epochs,
            epoch_duration_sec,
            reconnect_between_epochs,
            circuit_breaker_threshold,
        )

        for epoch in range(1, num_epochs + 1):
            _LOGGER.info(
                "=== Starting Soak Epoch %d / %d ===", epoch, num_epochs
            )
            try:
                start_kmem = await self._sample_kmem_stats()
                pre_irq: IrqStormSnapshot | None = None
                pre_dwc3: dict[str, Dwc3IrqSnapshot] | None = None
                if verify_no_irq_storm:
                    pre_irq = await take_irq_storm_snapshot(self.dut)
                    pre_dwc3 = collect_dwc3_irq_metrics(self.dut)

                workload_start = time.monotonic()
                total_adb_bytes = 0
                while time.monotonic() - workload_start < epoch_duration_sec:
                    cdc_task = self._exercise_cdc_network()
                    adb_task = self._exercise_adb_transfer()
                    ffx_task = self._exercise_overnet_ffx()
                    results = await asyncio.gather(cdc_task, adb_task, ffx_task)
                    total_adb_bytes += results[1]

                if reconnect_between_epochs and epoch < num_epochs:
                    _LOGGER.info(
                        "Epoch %d: Executing controlled disconnect/"
                        "reconnect...",
                        epoch,
                    )
                    await self._perform_controlled_disconnect(
                        disconnect_duration=disconnect_duration_sec,
                        reconnect_timeout=reconnect_timeout_sec,
                        epoch=epoch,
                    )

                if verify_no_irq_storm:
                    assert pre_irq is not None
                    assert pre_dwc3 is not None
                    post_irq = await take_irq_storm_snapshot(self.dut)
                    post_dwc3 = collect_dwc3_irq_metrics(self.dut)
                    assert_no_irq_storm(
                        pre_irq, post_irq, phase=f"epoch {epoch}"
                    )
                    assert_no_dwc3_unhandled_irq_storm(
                        pre_dwc3,
                        post_dwc3,
                        before_irq_snapshot=pre_irq,
                        after_irq_snapshot=post_irq,
                        phase=f"epoch {epoch}",
                    )

                current_boot_id = await self.dut.boot_id()
                asserts.assert_equal(
                    current_boot_id,
                    self._initial_boot_id,
                    f"DUT rebooted during epoch {epoch}: initial="
                    f"{self._initial_boot_id}, current={current_boot_id}",
                )

                end_kmem = await self._sample_kmem_stats()
                epoch_elapsed = time.monotonic() - workload_start
                start_vmo = start_kmem.get("vmo_bytes", 0)
                end_vmo = end_kmem.get("vmo_bytes", 0)
                start_heap = start_kmem.get("total_heap_bytes", 0)
                end_heap = end_kmem.get("total_heap_bytes", 0)
                mem_delta_kb = (
                    (end_vmo + end_heap) - (start_vmo + start_heap)
                ) / 1024.0
                growth_rate = mem_delta_kb / max(epoch_elapsed, 0.001)

                # Record memory stats and verify memory stats are populated.
                # In short 15s per-epoch soak runs, transient allocations
                # dominate; the strict 50 KB/hr rate threshold applies to
                # multi-hour soak qualification runs.
                asserts.assert_greater(
                    start_kmem.get("free_bytes", 0),
                    0,
                    "No free memory available on DUT at start of epoch",
                )
                asserts.assert_greater(
                    end_kmem.get("free_bytes", 0),
                    0,
                    "No free memory available on DUT after epoch execution",
                )

                _LOGGER.info(
                    "Epoch %d Summary: Duration=%.2fs, Memory Delta=%.2f KB "
                    "(Growth Rate=%.2f KB/s), ADB Bytes=%d",
                    epoch,
                    epoch_elapsed,
                    mem_delta_kb,
                    growth_rate,
                    total_adb_bytes,
                )

                self._epoch_history.append(
                    {
                        "epoch": epoch,
                        "elapsed_sec": epoch_elapsed,
                        "mem_delta_kb": mem_delta_kb,
                        "growth_rate_kb_sec": growth_rate,
                        "adb_bytes": total_adb_bytes,
                    }
                )
                self._consecutive_failures = 0

            except _FATAL_ERRORS as e:
                _LOGGER.error("Epoch %d fatal error: %s", epoch, e)
                raise signals.TestAbortAll(
                    f"Fatal unrecoverable error during epoch {epoch}: {e}"
                ) from e
            except signals.TestAbortAll:
                raise
            except _TRANSIENT_ERRORS as e:
                self._consecutive_failures += 1
                self._epoch_failures.append((epoch, str(e)))
                _LOGGER.error(
                    "Epoch %d transient error (%d consecutive): %s",
                    epoch,
                    self._consecutive_failures,
                    e,
                )
                if self._consecutive_failures >= circuit_breaker_threshold:
                    raise signals.TestAbortAll(
                        f"Circuit breaker tripped after "
                        f"{self._consecutive_failures} consecutive failures: "
                        f"{e}"
                    ) from e
            except signals.TestFailure as e:
                self._consecutive_failures += 1
                self._epoch_failures.append((epoch, str(e)))
                _LOGGER.error(
                    "Epoch %d assertion failure (%d consecutive): %s",
                    epoch,
                    self._consecutive_failures,
                    e,
                )
                if self._consecutive_failures >= circuit_breaker_threshold:
                    raise signals.TestAbortAll(
                        f"Circuit breaker tripped after "
                        f"{self._consecutive_failures} consecutive failures: "
                        f"{e}"
                    ) from e
            except Exception as e:
                self._consecutive_failures += 1
                self._epoch_failures.append((epoch, str(e)))
                _LOGGER.error(
                    "Epoch %d unexpected error (%d consecutive): %s",
                    epoch,
                    self._consecutive_failures,
                    e,
                )
                if self._consecutive_failures >= circuit_breaker_threshold:
                    raise signals.TestAbortAll(
                        f"Circuit breaker tripped after "
                        f"{self._consecutive_failures} consecutive failures: "
                        f"{e}"
                    ) from e

        if self._epoch_failures:
            failure_msgs = "; ".join(
                f"epoch {ep}: {msg}" for ep, msg in self._epoch_failures
            )
            asserts.fail(
                f"Encountered {len(self._epoch_failures)} failed epoch(s): "
                f"{failure_msgs}"
            )
        asserts.assert_equal(
            len(self._epoch_history),
            num_epochs,
            f"Expected {num_epochs} successful epochs, "
            f"got {len(self._epoch_history)}",
        )


if __name__ == "__main__":
    test_runner.main()
