# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import contextlib
import dataclasses
import importlib
import logging
import os
import pathlib
import weakref
from typing import AsyncGenerator

from honeydew.affordances.tracing import tracing as tracing_affordance
from honeydew.transports.ffx import ffx as ffx_transport
from honeydew.transports.ffx import types as ffx_types
from perf_publish import publish
from reporting import metrics
from trace_processing import trace_importing, trace_model
from trace_processing.metrics import odpm_power

_LOGGER = logging.getLogger(__name__)

ODPM_MONIKER = "bootstrap/boot-drivers:odpm-5228000"
ODPM_WLAN_RAIL = "wlan_bt"
ODPM_TRACE_CATEGORIES = ["kernel:meta", "power"]
# Number of hardware polls the ODPM driver accumulates and averages before
# emitting a single data point to Fuchsia trace (`*_odpm_rail` CounterEvent)
# and Inspect. Setting this to 1 emits a trace event on every hardware poll.
DEFAULT_POLLS_PER_SERIES_ENTRY = 1
DEFAULT_POLL_INTERVAL_MS = 1000

# Track Tracing instances (one per FuchsiaDevice / test class) that have already
# checked whether the ODPM component moniker is present on the device.
_odpm_available_by_tracing: weakref.WeakKeyDictionary[
    tracing_affordance.Tracing, bool
] = weakref.WeakKeyDictionary()

# Track Tracing instances that have already configured and logged the ODPM
# polling rate so it only runs the first time per test class.
_configured_polling_instances: weakref.WeakSet[
    tracing_affordance.Tracing
] = weakref.WeakSet()


def _get_ffx_from_tracing(
    tracing: tracing_affordance.Tracing,
) -> ffx_transport.FFX | None:
    """Extracts the FFX transport from a Honeydew Tracing affordance if available."""
    ffx = getattr(tracing, "_ffx", None)
    if ffx is not None:
        return ffx
    fc_transport = getattr(tracing, "_fc_transport", None)
    dut = getattr(fc_transport, "_device_address_change", None)
    return getattr(dut, "ffx", None)


def _is_odpm_available(tracing: tracing_affordance.Tracing) -> bool:
    """Checks once per Tracing instance whether the ODPM component is present."""
    if tracing in _odpm_available_by_tracing:
        return _odpm_available_by_tracing[tracing]

    ffx = _get_ffx_from_tracing(tracing)
    if ffx is None:
        _odpm_available_by_tracing[tracing] = True
        return True

    output = ffx.run(
        ["component", "list"],
        machine=ffx_types.MachineFormat.RAW,
    )
    available = any(
        line.strip() == ODPM_MONIKER for line in output.splitlines()
    )
    _odpm_available_by_tracing[tracing] = available
    return available


def _configure_polling_once(
    tracing: tracing_affordance.Tracing,
    polls_per_series_entry: int = DEFAULT_POLLS_PER_SERIES_ENTRY,
    poll_interval_ms: int = DEFAULT_POLL_INTERVAL_MS,
) -> None:
    """Configures and logs the ODPM driver polling rate.

    This will only run the first time it is called per test class, and skip
    configuration after that.
    """
    if tracing in _configured_polling_instances:
        return

    ffx = _get_ffx_from_tracing(tracing)
    if ffx is None:
        _configured_polling_instances.add(tracing)
        return

    cmd = (
        "odpm_util config set "
        f"--polls-per-series-entry {polls_per_series_entry} "
        f"--poll-interval-ms {poll_interval_ms}"
    )
    output = ffx.run(
        [
            "component",
            "explore",
            ODPM_MONIKER,
            "--command",
            cmd,
        ],
        machine=ffx_types.MachineFormat.RAW,
    )
    _configured_polling_instances.add(tracing)
    _LOGGER.debug(f"ODPM polling configuration:\n{output.strip()}")


async def start_trace(tracing: tracing_affordance.Tracing) -> None:
    """Starts a background trace session for ODPM.

    Raises:
        RuntimeError: If the ODPM component is not available on the device.
    """
    if not _is_odpm_available(tracing):
        raise RuntimeError(f"ODPM moniker {ODPM_MONIKER} not found on device.")
    _configure_polling_once(tracing)
    _LOGGER.info("Starting ODPM background trace...")
    if not tracing.is_session_initialized():
        tracing.initialize(categories=ODPM_TRACE_CATEGORIES)
    if not tracing.is_active():
        await tracing.start()


async def stop_trace(
    tracing: tracing_affordance.Tracing,
    trace_output_dir: str | os.PathLike[str],
) -> str:
    """Stops the ODPM background trace and returns the downloaded trace file path."""
    _LOGGER.info("Stopping ODPM background trace...")
    if tracing.is_active():
        await tracing.stop()
    return await tracing.terminate_and_download(
        directory=str(trace_output_dir),
        trace_file="odpm_trace.fxt",
    )


def get_odpm_measurements(
    trace_path: str | os.PathLike[str],
    rail: str = ODPM_WLAN_RAIL,
) -> list[metrics.TestCaseResult]:
    """Parses an ODPM trace file from a single test run and returns its power and energy metrics.

    Both returned `TestCaseResult` objects (`Power` and
    `TotalEnergy`) are generated together for a single test run.
    In `Power`, `values` contains the single duration-weighted mean
    power (in Watts) over that single test run, while `TotalEnergy`
    contains a single value for the total energy (in Joules) consumed during
    that same test run.

    Example returned results for a single test:
        [
            metrics.TestCaseResult(
                label="Power",
                unit=metrics.Unit.watts,
                values=[1.90],
                doc="ODPM average power usage for rails wlan_bt",
            ),
            metrics.TestCaseResult(
                label="TotalEnergy",
                unit=metrics.Unit.joules,
                values=[19.0],
                doc="Total energy consumed in Joules (MeanPower * trace duration)",
            ),
        ]

    Raises:
        FileNotFoundError: If `trace_path` does not exist.
        ValueError: If no ODPM power samples were found for `rail`.
    """
    _LOGGER.info("Collecting ODPM measurements from trace...")
    if not os.path.exists(trace_path):
        raise FileNotFoundError(f"Trace file does not exist: {trace_path}")

    model = trace_importing.create_model_from_trace_file_path(
        trace_path, without_json_conversion=True
    )
    processor = odpm_power.OdpmPowerMetricsProcessor(
        sum_rails={"wlan": [rail]},
    )
    results = [
        dataclasses.replace(r, label="Power")
        if r.label == "PowerAverage_wlan"
        else r
        for r in processor.process_metrics(model)
    ]

    # Calculate trace duration using the first and last ODPM events
    odpm_events = [
        e
        for e in model.all_events()
        if isinstance(e, trace_model.CounterEvent)
        and e.name.endswith("_odpm_rail")
    ]
    timestamps = [
        e.start.to_epoch_delta().to_nanoseconds() for e in odpm_events
    ]
    if timestamps:
        duration_sec = (max(timestamps) - min(timestamps)) / 1e9

        power_result = next(
            (r for r in results if r.label == "Power"),
            None,
        )
        if power_result and power_result.values:
            mean_power_val = power_result.values[0]
            if mean_power_val > 0.0:
                # Append the calculated total energy (in Joules) to `results`
                # alongside the `Power` (Watts) metric returned by
                # `processor.process_metrics(model)` above.
                results.append(
                    metrics.TestCaseResult(
                        label="TotalEnergy",
                        unit=metrics.Unit.joules,
                        values=[mean_power_val * duration_sec],
                        doc="Total energy consumed in Joules (MeanPower * trace duration)",
                    )
                )
    return results


async def stop_trace_and_save_metrics(
    tracing: tracing_affordance.Tracing,
    test_case_path: str | os.PathLike[str],
    test_name: str,
    rail: str = ODPM_WLAN_RAIL,
    save_raw_trace: bool = False,
) -> list[metrics.TestCaseResult]:
    """Stops ODPM background trace, processes metrics, and publishes fuchsiaperf JSON."""
    trace_path = await stop_trace(tracing, trace_output_dir=test_case_path)

    try:
        odpm_metrics = get_odpm_measurements(
            trace_path,
            rail=rail,
        )
    finally:
        # The temporary "odpm_trace.fxt" file is created uniquely by stop_trace()
        # specifically for ODPM power calculation. Delete it after metrics
        # processing to save disk space on infra test bots.
        if not save_raw_trace and os.path.exists(trace_path):
            os.remove(trace_path)

    test_suite = f"fuchsia.wlan.{test_name}"
    fuchsiaperf_path = (
        pathlib.Path(test_case_path) / "power_odpm.fuchsiaperf.json"
    )
    metrics.TestCaseResult.write_fuchsiaperf_json(
        odpm_metrics,
        test_suite=test_suite,
        output_path=fuchsiaperf_path,
    )
    publish.publish_fuchsiaperf(
        fuchsia_perf_file_paths=[fuchsiaperf_path],
        expected_metric_names_filename=f"{test_suite}.txt",
        test_data_module=importlib.import_module("test_data"),
        runtime_deps_dir=test_case_path,
    )

    return odpm_metrics


@contextlib.asynccontextmanager
async def record_odpm_metrics(
    tracing: tracing_affordance.Tracing,
    test_case_path: str | os.PathLike[str],
    test_name: str,
    rail: str = ODPM_WLAN_RAIL,
    save_raw_trace: bool = False,
) -> AsyncGenerator[None, None]:
    """Context manager that records ODPM trace metrics for a specific block."""
    if not _is_odpm_available(tracing):
        _LOGGER.warning(
            f"ODPM moniker {ODPM_MONIKER} not found on device; skipping ODPM metrics."
        )
        yield
        return

    await start_trace(tracing)
    succeeded = False
    try:
        yield
        # This line is not reached if an exception (such as a test failure) is
        # raised inside the `async with` block, preventing partial metrics from
        # being published in `finally`.
        succeeded = True
    finally:
        if succeeded:
            await stop_trace_and_save_metrics(
                tracing,
                test_case_path=test_case_path,
                test_name=test_name,
                rail=rail,
                save_raw_trace=save_raw_trace,
            )
        else:
            # An exception from the `yield` body is already propagating. Log and
            # suppress tracing cleanup errors (e.g. if the device crashed or
            # disconnected) so they do not mask the original test failure.
            try:
                if save_raw_trace:
                    await stop_trace(tracing, trace_output_dir=test_case_path)
                else:
                    await tracing.terminate()
            except Exception as e:
                _LOGGER.warning(
                    f"Failed to clean up ODPM trace after test failure: {e}"
                )
