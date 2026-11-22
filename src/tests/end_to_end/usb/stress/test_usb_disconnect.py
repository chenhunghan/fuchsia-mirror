# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""USB Disconnect stress tests (handles both physical and virtual)."""

import asyncio
import json
import logging
import math
import random
import re

import usb_lib
from honeydew import errors
from honeydew.device_classes import android_device
from honeydew.transports.ffx import errors as ffx_errors
from honeydew.typing import custom_types
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
from usb_lib.usb_cli_diag import find_dut_diag_problems
from usb_lib.usb_traffic import check_adb_traffic, check_cdc_traffic

_LOGGER: logging.Logger = logging.getLogger(__name__)

_DEFAULT_RECONNECT_TIMEOUT_SEC: int = 60
_DEFAULT_DISCONNECT_DURATION_SEC: float = 10.0
_DEFAULT_TEST_CASE_PREFIX: str = "test_usb_disconnect"
_DISCONNECT_METHOD_POWER_CYCLE: str = "power_cycle"
_DISCONNECT_METHOD_RESET: str = "reset"
_DEFAULT_TRAFFIC_FUNCTIONS: list[str] = ["cdc", "adb"]
_DEFAULT_TRAFFIC_SIZE_MB: int = 5
_DURATION_PATTERN: re.Pattern[str] = re.compile(
    r"^\s*(\d+(?:\.\d+)?)\s*(ns|us|ms|s)?\s*$"
)


def _parse_duration_sec(value: object, name: str) -> float:
    """Parses a non-negative duration into seconds.

    Accepts numbers (interpreted as seconds) or strings with an optional unit
    suffix (`ns`, `us`, `ms`, or `s`, e.g. `"500us"`, `"20ms"`, `"2s"`, `"10"`).
    Supporting unit-suffixed strings allows `BUILD.gn` (which has no
    floating-point type) to specify sub-second and sub-millisecond durations.

    Raises:
        signals.TestAbortClass: `value` is not a valid non-negative duration.
    """
    if isinstance(value, bool):
        raise signals.TestAbortClass(
            f"{name} must be a non-negative duration, got {value!r}"
        )
    if isinstance(value, (int, float)):
        seconds = float(value)
        if not math.isfinite(seconds) or seconds < 0:
            raise signals.TestAbortClass(
                f"{name} must be a non-negative finite number, got {value!r}"
            )
        return seconds
    if isinstance(value, str):
        match = _DURATION_PATTERN.match(value)
        if match is not None:
            amount = float(match.group(1))
            unit = match.group(2) or "s"
            if unit == "ns":
                scale = 1e-9
            elif unit == "us":
                scale = 1e-6
            elif unit == "ms":
                scale = 1e-3
            else:
                scale = 1.0
            seconds = amount * scale
            if math.isfinite(seconds):
                return seconds
    raise signals.TestAbortClass(
        f"{name} must be a non-negative number (seconds) or duration string "
        f"(e.g. '500us', '20ms', '2s'), got {value!r}"
    )


def _format_duration_sec(seconds: float) -> str:
    """Formats a duration in seconds with an appropriate human-readable unit."""
    if seconds == 0 or seconds >= 1.0:
        return f"{seconds:.1f}s"
    if seconds >= 1e-3:
        return f"{seconds * 1e3:.1f}ms"
    if seconds >= 1e-6:
        return f"{seconds * 1e6:.1f}us"
    return f"{seconds * 1e9:.0f}ns"


def _format_duration_exact(seconds: float) -> str:
    """Formats a duration as an exact unit-suffixed string accepted by `_parse_duration_sec`."""
    nanos = round(seconds * 1_000_000_000)
    if nanos == 0:
        return "0s"
    if nanos % 1_000_000_000 == 0:
        return f"{nanos // 1_000_000_000}s"
    if nanos % 1_000_000 == 0:
        return f"{nanos // 1_000_000}ms"
    if nanos % 1_000 == 0:
        return f"{nanos // 1_000}us"
    return f"{nanos}ns"


def _format_duration_log(seconds: float) -> str:
    """Formats a duration for logs with both human-readable and exact repro forms."""
    exact = _format_duration_exact(seconds)
    human = _format_duration_sec(seconds)
    if _parse_duration_sec(human, "human") == _parse_duration_sec(
        exact, "exact"
    ):
        return exact
    return f"{human} ({exact})"


class _StratifiedDurationSampler:
    """Draws durations evenly across one or more `[min, max]` buckets.

    Visits every bucket once per pass in a shuffled order (using the shared
    seeded RNG) before starting the next pass, guaranteeing balanced coverage
    across all configured duration regimes while keeping random jitter inside
    each bucket.
    """

    def __init__(
        self, buckets: list[tuple[float, float]], rng: random.Random
    ) -> None:
        self._buckets: list[tuple[float, float]] = list(buckets)
        self._rng: random.Random = rng
        self._order: list[int] = []
        self._cursor: int = 0

    @property
    def buckets(self) -> tuple[tuple[float, float], ...]:
        return tuple(self._buckets)

    @property
    def min_bound(self) -> float:
        return min(lo for lo, _ in self._buckets)

    def draw(self) -> tuple[float, tuple[float, float]]:
        """Returns `(duration_sec, bucket)` for the next iteration."""
        if self._cursor >= len(self._order):
            self._order = list(range(len(self._buckets)))
            if len(self._order) > 1:
                self._rng.shuffle(self._order)
            self._cursor = 0
        bucket = self._buckets[self._order[self._cursor]]
        self._cursor += 1
        return self._rng.uniform(*bucket), bucket


def _format_bucket_exact(bucket: tuple[float, float]) -> list[str]:
    """Formats a `(min, max)` bucket as exact duration strings for logging/records."""
    return [
        _format_duration_exact(bucket[0]),
        _format_duration_exact(bucket[1]),
    ]


class UsbDisconnectTest(usb_lib.UsbPowerHubBaseTest):
    """Mobly test for testing USB disconnects.

    Supports both physical disconnect (using hardware PDU/power hub) and virtual
    disconnect (using software authorization control).

    Required Mobly Test Params:
        num_iterations (int, optional): Number of times to execute the test.
            Defaults to 10 (or uses num_usb_disconnects if provided).
        num_usb_disconnects (int, optional): Alias for num_iterations.
        disconnect_duration (float | str, optional): How long to stay
            disconnected. Accepts a number of seconds or a unit-suffixed string
            (`ns`, `us`, `ms`, `s`, e.g. `"500us"`, `"20ms"`, `"2s"`). Also
            accepted under the legacy name `disconnect_duration_sec`. Defaults
            to 10s. Not used by the "reset" disconnect method.
        dwell_ranges (list[list[float | str]], optional): List of `[min, max]`
            (or `[x]`) duration buckets, where each bound is seconds or a
            unit-suffixed duration string (e.g. `[["500ms", "2s"], ["2s", "8s"],
            ["8s", "20s"]]`). Across iterations, the test cycles evenly through
            all buckets in a per-pass shuffled order (seeded by `timing_seed`)
            and draws uniformly within the selected bucket, so short regimes are
            not starved by wider ranges. Mutually exclusive with `dwell_range` /
            `dwell_range_sec`.
        dwell_range (list[float | str] | float | str, optional): Single-bucket
            shorthand for `dwell_ranges`: `[min, max]`, where each bound is
            seconds or a unit-suffixed duration string (e.g. `["500us", "250ms"]`
            or `["2s", "20s"]`). Also accepts a single value (e.g.
            `"45612345ns"`, `5`, or `["500us"]`) as shorthand for `[x, x]` to
            reproduce an exact logged dwell. Also accepted as `dwell_range_sec`.
        settle_delay_ranges (list[list[float | str]], optional): List of
            `[min, max]` (or `[x]`) duration buckets applied before **each**
            disconnect iteration (while the DUT is online), cycled evenly in an
            independent per-pass shuffled order from `dwell_ranges` so runs
            exercise varied `(settle, dwell)` combinations (from back-to-back
            replugs to idle/autosuspend windows). Mutually exclusive with
            `settle_delay_range` / `settle_delay_range_sec`.
        settle_delay_range (list[float | str] | float | str, optional):
            Single-bucket shorthand for `settle_delay_ranges`: `[min, max]`, or
            a single value (e.g. `"5s"` or `["5s"]`) as shorthand for `[x, x]`
            to reproduce an exact logged per-iteration settle delay. Also
            accepted as `settle_delay_range_sec`.
        timing_seed (int, optional): Seed of the random generator behind
            `dwell_ranges`/`dwell_range` and `settle_delay_ranges`/
            `settle_delay_range`. A run logs the seed it used and records it in
            the test summary, so that a failure can be replayed with the same
            timings.
        disconnect_method (str, optional): How to disconnect the device:
            - "power_cycle" (default): power the USB port off, wait for the
              device to go offline and `disconnect_duration_sec`, then power
              it back on.
            - "reset": reset the USB link in a single step, e.g. with
              `virtual_usb_hub_mode: reset`. The link has no "off" phase, so
              there is no offline wait nor disconnect duration. Hubs that
              cannot reset fall back to a power cycle without the offline
              wait.
        reconnect_timeout_sec (int, optional): How long to wait for the device
            to come back online after USB power is restored. Defaults to 60.
        verify_mdns (bool, optional): Verify that the device is discoverable
            over mDNS before the first disconnect and after every reconnect.
            Defaults to False.
        mdns_timeout_sec (int, optional): How long each mDNS check waits for
            the device to be discovered. Defaults to 12, which allows one
            retry, since ffx re-sends its mDNS query every 10s.
        expected_usb_link_speed (str, optional): When set, e.g. to "high",
            assert the USB link negotiated this speed after every reconnect.
        verify_no_irq_storm (bool, optional): Verify that no driver dispatcher
            spin-loop or unhandled IRQ storm occurred across each disconnect
            cycle (checked across all drivers on the DUT, not only USB
            drivers). Defaults to False.
        verify_traffic (bool, optional): After every reconnect, move
            `traffic_size_mb` of data over each function in
            `traffic_functions` and then check that `usb-cli diag` shows no
            health error or stuck DWC3 endpoint. A control-plane ping alone
            can pass on a wedged Bulk endpoint. The transfer runs after the
            IRQ storm checks, so an IRQ storm that the traffic itself
            triggers only shows up in the next iteration's window. Defaults
            to False.
        traffic_functions (list[str], optional): Which USB functions
            `verify_traffic` exercises, out of "cdc" (an SSH transfer) and
            "adb" (a push and pull, which needs an Android DUT). Defaults to
            both.
        traffic_size_mb (int, optional): How many MiB `verify_traffic` moves
            over each function. Defaults to 5.
        test_case_prefix (str, optional): Prefix of the generated test case
            names, which end in the iteration number. Defaults to
            "test_usb_disconnect".
    """

    USB_POWER_HUB_REQUIRED: bool = True
    _reconnect_failed: bool = False
    _rng: random.Random
    _timing_seed: int
    _disconnect_duration: float = _DEFAULT_DISCONNECT_DURATION_SEC
    _dwell_sampler: _StratifiedDurationSampler | None = None
    _settle_sampler: _StratifiedDurationSampler | None = None
    _traffic_functions: list[str] = []
    _traffic_size_bytes: int = 0

    async def pre_run(self) -> None:
        """Mobly method used to generate the test cases at run time."""
        test_arg_tuple_list: list[tuple[int]] = []

        num_iterations = int(
            self.user_params.get(
                "num_iterations",
                self.user_params.get("num_usb_disconnects", 10),
            )
        )
        for iteration in range(1, num_iterations + 1):
            test_arg_tuple_list.append((iteration,))

        self.generate_tests(
            test_logic=self._test_logic,
            name_func=self._name_func,
            arg_sets=test_arg_tuple_list,
        )

    async def setup_class(self) -> None:
        """setup_class is called once before running tests."""
        await super().setup_class()
        self._init_timing_rng()
        # Reject bad params before the baseline checks, which can take minutes.
        self._disconnect_duration = self._parse_disconnect_duration()
        dwell_buckets = self._duration_buckets(
            "dwell_ranges", "dwell_range", "dwell_range_sec"
        )
        self._dwell_sampler = (
            _StratifiedDurationSampler(dwell_buckets, self._rng)
            if dwell_buckets is not None
            else None
        )
        settle_buckets = self._duration_buckets(
            "settle_delay_ranges",
            "settle_delay_range",
            "settle_delay_range_sec",
        )
        self._settle_sampler = (
            _StratifiedDurationSampler(settle_buckets, self._rng)
            if settle_buckets is not None
            else None
        )
        self._traffic_functions = self._parse_traffic_functions()
        self._traffic_size_bytes = self._parse_traffic_size_bytes()
        if self.user_params.get("verify_traffic", False):
            if "adb" in self._traffic_functions:
                if not isinstance(self.dut, android_device.AndroidDevice):
                    raise signals.TestAbortClass(
                        "traffic_functions includes adb, but the DUT is not "
                        "an AndroidDevice"
                    )
                # Honeydew can only set up ADB while the DUT exposes it.
                _ = self.dut.adb
            # Confirm traffic flows before any disconnect, so that a failure
            # in an iteration can be attributed to the hotplug.
            await asyncio.to_thread(self._verify_traffic, "baseline")
        if self.user_params.get("verify_mdns", False):
            # Confirm mDNS works before any disconnect, so that a failure in
            # an iteration can be attributed to the hotplug.
            self._verify_mdns_advertisement("baseline")

    def _init_timing_rng(self) -> None:
        """Seeds the generator behind the random settle and dwell times."""
        seed = self.user_params.get("timing_seed")
        self._timing_seed = (
            random.SystemRandom().randrange(1 << 31)
            if seed is None
            else int(seed)
        )
        self._rng = random.Random(self._timing_seed)
        _LOGGER.info(
            "Using timing_seed=%d (pass it as a test param to replay)",
            self._timing_seed,
        )
        self.record_data(
            {"Test Class": self.TAG, "timing_seed": self._timing_seed}
        )

    def _get_aliased_param(
        self, name: str, legacy_name: str
    ) -> tuple[str, object] | None:
        """Returns `(param_name, value)` for `name` or `legacy_name` if set."""
        value = self.user_params.get(name)
        legacy_value = self.user_params.get(legacy_name)
        if value is not None and legacy_value is not None:
            raise signals.TestAbortClass(
                f"Specify only one of {name} or {legacy_name}"
            )
        if value is not None:
            return name, value
        if legacy_value is not None:
            return legacy_name, legacy_value
        return None

    def _parse_disconnect_duration(self) -> float:
        entry = self._get_aliased_param(
            "disconnect_duration", "disconnect_duration_sec"
        )
        if entry is None:
            return _DEFAULT_DISCONNECT_DURATION_SEC
        param_name, raw_value = entry
        return _parse_duration_sec(raw_value, param_name)

    def _parse_duration_bucket(
        self, bounds: object, param_name: str, *, allow_scalar: bool
    ) -> tuple[float, float]:
        """Parses a single `[min, max]` (or `[x]` / scalar) duration bucket."""
        if not isinstance(bounds, (list, tuple)):
            if not allow_scalar:
                raise signals.TestAbortClass(
                    f"{param_name} must be a [min, max] or [x] duration list, "
                    f"got {bounds!r}"
                )
            value = _parse_duration_sec(bounds, param_name)
            return value, value
        if len(bounds) == 1:
            value = _parse_duration_sec(bounds[0], f"{param_name}[0]")
            return value, value
        if len(bounds) != 2:
            raise signals.TestAbortClass(
                f"{param_name} must be a [min, max] duration with "
                f"0 <= min <= max, got {bounds!r}"
            )
        lo = _parse_duration_sec(bounds[0], f"{param_name}[0]")
        hi = _parse_duration_sec(bounds[1], f"{param_name}[1]")
        if lo > hi:
            raise signals.TestAbortClass(
                f"{param_name} must be a [min, max] duration with "
                f"0 <= min <= max, got {bounds!r}"
            )
        return lo, hi

    def _duration_buckets(
        self, plural_name: str, singular_name: str, legacy_singular_name: str
    ) -> list[tuple[float, float]] | None:
        """Returns a list of `[min, max]` duration buckets in seconds, or None.

        Accepts either `<plural_name>` (non-empty list of `[min, max]` or `[x]`
        lists) or `<singular_name>` / `<legacy_singular_name>` (single `[min, max]`,
        `[x]`, or scalar duration).

        Raises:
            signals.TestAbortClass: Multiple aliases are set or malformed.
        """
        candidates = [
            (name, self.user_params.get(name))
            for name in (plural_name, singular_name, legacy_singular_name)
            if self.user_params.get(name) is not None
        ]
        if not candidates:
            return None
        if len(candidates) > 1:
            names = ", ".join(name for name, _ in candidates)
            raise signals.TestAbortClass(f"Specify only one of {names}")
        param_name, raw_value = candidates[0]
        if param_name == plural_name:
            if not isinstance(raw_value, (list, tuple)) or not raw_value:
                raise signals.TestAbortClass(
                    f"{param_name} must be a non-empty list of [min, max] "
                    f"duration buckets, got {raw_value!r}"
                )
            return [
                self._parse_duration_bucket(
                    bucket, f"{param_name}[{idx}]", allow_scalar=False
                )
                for idx, bucket in enumerate(raw_value)
            ]
        return [
            self._parse_duration_bucket(
                raw_value, param_name, allow_scalar=True
            )
        ]

    def _parse_traffic_functions(self) -> list[str]:
        functions = self.user_params.get(
            "traffic_functions", _DEFAULT_TRAFFIC_FUNCTIONS
        )
        if (
            not isinstance(functions, (list, tuple))
            or not functions
            or not set(functions) <= {"cdc", "adb"}
        ):
            raise signals.TestAbortClass(
                "traffic_functions must be a non-empty subset of "
                f"['cdc', 'adb'], got {functions!r}"
            )
        return list(functions)

    def _parse_traffic_size_bytes(self) -> int:
        size_mb = self.user_params.get(
            "traffic_size_mb", _DEFAULT_TRAFFIC_SIZE_MB
        )
        if (
            isinstance(size_mb, bool)
            or not isinstance(size_mb, int)
            or size_mb < 1
        ):
            raise signals.TestAbortClass(
                f"traffic_size_mb must be a positive integer, got {size_mb!r}"
            )
        return size_mb * 1024 * 1024

    def _verify_traffic(self, phase: str) -> None:
        """Moves data over each traffic function, then checks `usb-cli diag`.

        The diag check comes last, so that the TRB rings have just carried
        traffic. It runs over SSH, falling back to serial if that fails.
        """
        size_bytes = self._traffic_size_bytes
        if "cdc" in self._traffic_functions:
            check_cdc_traffic(self.dut, size_bytes=size_bytes, phase=phase)
        if "adb" in self._traffic_functions:
            check_adb_traffic(self.dut, size_bytes=size_bytes, phase=phase)
        problems, _ = find_dut_diag_problems(self.dut, over_ssh=True)
        asserts.assert_false(problems, f"[{phase}] usb-cli diag: {problems}")

    async def setup_test(self) -> None:
        """setup_test is called once before running each test."""
        self._reconnect_failed = False
        await super().setup_test()

    async def teardown_test(self) -> None:
        """teardown_test is called once after running each test."""
        if self._reconnect_failed:
            _LOGGER.warning(
                "Skipping teardown health check because device failed to reconnect over USB."
            )
            return
        await super().teardown_test()

    async def _test_logic(self, iteration: int) -> None:
        """Test case logic that disconnects the USB from a fuchsia device."""
        _LOGGER.info(
            "Starting the Usb Disconnect test iteration# %s", iteration
        )

        disconnect_duration: float = self._disconnect_duration
        dwell_bucket: tuple[float, float] | None = None
        if self._dwell_sampler is not None:
            disconnect_duration, dwell_bucket = self._dwell_sampler.draw()
        settle_delay: float | None = None
        settle_bucket: tuple[float, float] | None = None
        if self._settle_sampler is not None:
            settle_delay, settle_bucket = self._settle_sampler.draw()
        disconnect_method = self.user_params.get(
            "disconnect_method", _DISCONNECT_METHOD_POWER_CYCLE
        )
        if disconnect_method not in (
            _DISCONNECT_METHOD_POWER_CYCLE,
            _DISCONNECT_METHOD_RESET,
        ):
            raise signals.TestAbortAll(
                f"Unsupported disconnect_method {disconnect_method!r}; "
                f"expected {_DISCONNECT_METHOD_POWER_CYCLE!r} or "
                f"{_DISCONNECT_METHOD_RESET!r}."
            )
        timing_record: dict[str, object] = {
            "Test Class": self.TAG,
            "iteration": iteration,
            "timing_seed": self._timing_seed,
        }
        if settle_delay is not None:
            exact_settle = _format_duration_exact(settle_delay)
            _LOGGER.info(
                "Iteration %d settle delay: %s "
                "(bucket=%s; timing_seed=%d; repro: settle_delay_range=%r)",
                iteration,
                _format_duration_log(settle_delay),
                _format_bucket_exact(settle_bucket)
                if settle_bucket is not None
                else None,
                self._timing_seed,
                exact_settle,
            )
            timing_record["settle_delay"] = exact_settle
            timing_record["settle_delay_sec"] = settle_delay
            if settle_bucket is not None:
                timing_record["settle_bucket"] = _format_bucket_exact(
                    settle_bucket
                )
        if disconnect_method != _DISCONNECT_METHOD_RESET:
            exact_dwell = _format_duration_exact(disconnect_duration)
            _LOGGER.info(
                "Iteration %d dwell time: %s "
                "(bucket=%s; timing_seed=%d; repro: dwell_range=%r)",
                iteration,
                _format_duration_log(disconnect_duration),
                _format_bucket_exact(dwell_bucket)
                if dwell_bucket is not None
                else None,
                self._timing_seed,
                exact_dwell,
            )
            timing_record["dwell"] = exact_dwell
            timing_record["dwell_sec"] = disconnect_duration
            if dwell_bucket is not None:
                timing_record["dwell_bucket"] = _format_bucket_exact(
                    dwell_bucket
                )
        if (
            settle_delay is not None
            or disconnect_method != _DISCONNECT_METHOD_RESET
        ):
            self.record_data(timing_record)
        reconnect_timeout = int(
            self.user_params.get(
                "reconnect_timeout_sec", _DEFAULT_RECONNECT_TIMEOUT_SEC
            )
        )

        power_hub = self.require_usb_power_hub()

        try:
            await asyncio.wait_for(
                self.dut.wait_for_online(),
                timeout=reconnect_timeout,
            )
        except asyncio.TimeoutError as e:
            self._reconnect_failed = True
            raise signals.TestAbortAll(
                f"Device {self.dut.device_name} failed to come online before "
                f"starting iteration {iteration} within {reconnect_timeout}s."
            ) from e

        if settle_delay is not None and settle_delay > 0:
            _LOGGER.info(
                "Settling for %s before iteration %d disconnect...",
                _format_duration_log(settle_delay),
                iteration,
            )
            await asyncio.sleep(settle_delay)

        pre_disconnect_boot_id = await self.dut.boot_id()
        _LOGGER.info("Pre-disconnect Boot ID: %s", pre_disconnect_boot_id)
        verify_no_irq_storm = bool(
            self.user_params.get("verify_no_irq_storm", False)
        )
        pre_disconnect_irq_snapshot: IrqStormSnapshot | None = None
        pre_disconnect_dwc3_metrics: dict[str, Dwc3IrqSnapshot] | None = None
        if verify_no_irq_storm:
            pre_disconnect_irq_snapshot = await take_irq_storm_snapshot(
                self.dut
            )
            pre_disconnect_dwc3_metrics = collect_dwc3_irq_metrics(self.dut)
        try:
            self.dut.usb.before_disconnect()
            if disconnect_method == _DISCONNECT_METHOD_RESET:
                _LOGGER.info("Resetting the USB link...")
                await asyncio.to_thread(
                    usb_lib.reset_usb_link,
                    power_hub,
                    disconnect_duration,
                )
                # An in-place reset keeps the USB device number, so a running
                # host ADB server keeps a stale handle and loses the device.
                # Restart only the host server; adbd on the DUT is unaffected.
                # Called on the main thread, not via asyncio.to_thread(),
                # since it runs the health check, which sets a SIGALRM.
                if isinstance(self.dut, android_device.AndroidDevice):
                    try:
                        adb = self.dut.adb
                    except (errors.NotSupportedError, errors.NotEnabledError):
                        # No Honeydew ADB transport, so no host server to
                        # restart.
                        pass
                    else:
                        adb.restart_server()
            else:
                power_hub.power_off()
                _LOGGER.info("Waiting for the device to go offline...")
                await asyncio.to_thread(self.dut.wait_for_offline)
                _LOGGER.info("Device is successfully offline.")

                if disconnect_duration > 0:
                    _LOGGER.info(
                        "Sleeping for %s...",
                        _format_duration_sec(disconnect_duration),
                    )
                    await asyncio.sleep(disconnect_duration)
        finally:
            if disconnect_method == _DISCONNECT_METHOD_POWER_CYCLE:
                power_hub.power_on()
            _LOGGER.info("Waiting for the device to go online...")
            try:
                await asyncio.wait_for(
                    self.dut.wait_for_online(),
                    timeout=reconnect_timeout,
                )
            except asyncio.TimeoutError as e:
                self._reconnect_failed = True
                raise signals.TestAbortAll(
                    f"Device {self.dut.device_name} failed to reconnect over USB "
                    f"network within {reconnect_timeout}s after USB power-on."
                ) from e
            self.dut.usb.after_connect()
            post_reconnect_boot_id = await self.dut.boot_id()
            _LOGGER.info("Post-reconnect Boot ID: %s", post_reconnect_boot_id)
            if pre_disconnect_boot_id != post_reconnect_boot_id:
                raise errors.FuchsiaDeviceError(
                    f"Unexpected reboot detected during USB disconnect for {self.dut.device_name}. "
                    f"Boot ID before: {pre_disconnect_boot_id} != after: {post_reconnect_boot_id}"
                )
            _LOGGER.info("Device is successfully back online.")
            self.dut.health_check()

        # Outside the `finally` block so that an mDNS, link speed, IRQ storm,
        # or traffic failure cannot mask a disconnect failure.
        if (
            pre_disconnect_irq_snapshot is not None
            and pre_disconnect_dwc3_metrics is not None
        ):
            # Note: `assert_no_irq_storm` checks for driver dispatcher
            # spin-loops and hardware IRQ storms across all drivers on the DUT
            # (not only USB drivers).
            post_reconnect_irq_snapshot = await take_irq_storm_snapshot(
                self.dut
            )
            post_reconnect_dwc3_metrics = collect_dwc3_irq_metrics(self.dut)
            assert_no_irq_storm(
                pre_disconnect_irq_snapshot,
                post_reconnect_irq_snapshot,
                phase=f"iteration {iteration}",
            )
            assert_no_dwc3_unhandled_irq_storm(
                pre_disconnect_dwc3_metrics,
                post_reconnect_dwc3_metrics,
                before_irq_snapshot=pre_disconnect_irq_snapshot,
                after_irq_snapshot=post_reconnect_irq_snapshot,
                phase=f"iteration {iteration}",
            )
        if self.user_params.get("verify_mdns", False):
            self._verify_mdns_advertisement(f"iteration {iteration}")
        if expected_speed := self.user_params.get("expected_usb_link_speed"):
            assert_link_speed(
                self.dut,
                expected=expected_speed,
                phase=f"iteration {iteration}",
            )
        if self.user_params.get("verify_traffic", False):
            # Last, so that it runs on a link that passed every other check,
            # and so that the IRQ window above isn't skewed by the transfer.
            await asyncio.to_thread(
                self._verify_traffic, f"iteration {iteration}"
            )

        _LOGGER.info(
            "Successfully ended the Usb Disconnect test iteration# %s",
            iteration,
        )

    def _verify_mdns_advertisement(self, phase: str) -> None:
        """Asserts that the device is discoverable over mDNS.

        In infra, Honeydew reconnects to the device by static IP, and ffx can
        also find it over USB, so a broken mDNS responder would otherwise go
        unnoticed. `--no-usb` limits discovery to mDNS, and `--no-probe` skips
        connecting to the device since only discovery is under test.

        Args:
            phase: Label used in log and failure messages, e.g. "iteration 3".
        """
        timeout_sec = int(self.user_params.get("mdns_timeout_sec", 12))
        try:
            output = self.dut.ffx.run(
                cmd=[
                    "-c",
                    f"discovery.timeout={timeout_sec * 1000}",
                    "target",
                    "list",
                    "--no-usb",
                    "--no-probe",
                    self.dut.device_name,
                ],
                include_target=False,
            )
        except ffx_errors.FfxCommandError as e:
            # When the named device is not discovered, ffx exits with an error
            # rather than returning an empty list.
            raise signals.TestFailure(
                f"[{phase}] Device {self.dut.device_name} not discovered via "
                f"mDNS within {timeout_sec}s: {e}"
            ) from e

        targets = json.loads(output)
        asserts.assert_true(
            bool(targets), f"[{phase}] No targets returned by ffx target list"
        )
        discovered_ips = [
            str(address["ip"]).split("%")[0]
            for address in targets[0]["addresses"]
        ]
        _LOGGER.info(
            "[%s] Device discovered via mDNS at %s", phase, discovered_ips
        )

        target_addr = self.dut.ffx.get_target_address()
        if isinstance(target_addr, custom_types.IpPort):
            expected_ip = str(target_addr.ip).split("%")[0]
            asserts.assert_in(
                expected_ip,
                discovered_ips,
                f"[{phase}] Expected DUT IP {expected_ip} in mDNS addresses "
                f"{discovered_ips}",
            )

    def _name_func(self, iteration: int) -> str:
        """This function generates the names of each test case based on each
        argument set.

        The name function should have the same signature as the actual test
        logic function.

        Returns:
            Test case name
        """
        prefix = self.user_params.get(
            "test_case_prefix", _DEFAULT_TEST_CASE_PREFIX
        )
        return f"{prefix}_{iteration}"


if __name__ == "__main__":
    test_runner.main()
