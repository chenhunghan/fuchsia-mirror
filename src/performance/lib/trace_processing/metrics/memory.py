#!/usr/bin/env fuchsia-vendored-python
# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Computes metrics from memory traces."""

import collections
import dataclasses
import typing
from collections.abc import Collection

from reporting import metrics
from trace_processing import trace_metrics, trace_model, trace_time, trace_utils

MEMORY_SYSTEM_CATEGORY = "memory:kernel"
_NS_PER_MS = 1_000_000.0

# The names of threads that perform memory management tasks; CPU time spent in
# these threads gets aggregated into a "management time" cumulative metric.
_MEMORY_MANAGEMENT_THREAD_NAMES = frozenset(
    (
        # TODO(https://fxbug.dev/555184304): Scale computed eviction-thread
        # management time by core count?
        "eviction-thread",
        "kernel-memory-reclaim",
        "memory-pressure-thread",
        "page-queue-lru-thread",
        "page-queue-mru-thread",
        "scanner-request-thread",
        "stall-aggregator",
    )
)

_KERNEL_EVENT_BASIC = "kmem_stats_a"
_KERNEL_EVENT_NAMES = (
    _KERNEL_EVENT_BASIC,
    "kmem_stats_b",
    "kmem_stats_compression",
    "memory_stall",
    "page_request_wait",
)
_KERNEL_EVENT_NAMES_SET = frozenset(_KERNEL_EVENT_NAMES)
# Names of some of the metrics that are cumulative, monotonic counters,
# as opposed to gauges.
_CUMULATIVE_METRIC_NAMES = frozenset(
    {
        "compression_time",
        "decompression_time",
        "stall_time_some_ns",
        "stall_time_full_ns",
        "page_refaults",
        "page_faults",
    }
)


@dataclasses.dataclass(frozen=True)
class _StructuredMetricName:
    structured_name: str
    unit: metrics.Unit
    direction: metrics.Direction | None = None


# Names and units of some of the metrics we will export as structured metrics.
# The key is the name of the metric in the trace, and the value is a StructuredMetricName object.
_STRUCTURED_METRIC_NAMES = {
    "stall_time_some_ns": _StructuredMetricName(
        "Memory/System/StallTimeSome", metrics.Unit.nanoseconds
    ),
    "stall_time_full_ns": _StructuredMetricName(
        "Memory/System/StallTimeFull", metrics.Unit.nanoseconds
    ),
    "compression_time": _StructuredMetricName(
        "Memory/System/CompressionTime", metrics.Unit.nanoseconds
    ),
    "decompression_time": _StructuredMetricName(
        "Memory/System/DecompressionTime", metrics.Unit.nanoseconds
    ),
    # Both page_refaults and page_faults are measured in pages supplied by
    # userspace pagers (including readahead).
    "page_refaults": _StructuredMetricName(
        "Memory/System/PageRefaults",
        metrics.Unit.count,
        metrics.Direction.smallerIsBetter,
    ),
    "page_faults": _StructuredMetricName(
        "Memory/System/PageFaults/Major",
        metrics.Unit.count,
        metrics.Direction.smallerIsBetter,
    ),
    "page_request_wait_read": _StructuredMetricName(
        "Memory/System/PageRequestTime/Read",
        metrics.Unit.nanoseconds,
        metrics.Direction.smallerIsBetter,
    ),
    "page_request_wait_dirty": _StructuredMetricName(
        "Memory/System/PageRequestTime/Dirty",
        metrics.Unit.nanoseconds,
        metrics.Direction.smallerIsBetter,
    ),
    "total_heap_bytes": _StructuredMetricName(
        "Memory/System/ZirconHeapBytes", metrics.Unit.bytes
    ),
}

_MANAGEMENT_STRUCTURED_METRIC_NAME = _StructuredMetricName(
    "Memory/System/ManagementTime", metrics.Unit.milliseconds
)

# Fields from `zx_info_kmem_stats` composing the total kernel private memory.
_KERNEL_PRIVATE_MEMORY_FIELDS = (
    "wired_bytes",
    "total_heap_bytes",
    "mmu_overhead_bytes",
    "ipc_bytes",
    "cache_bytes",
    "slab_bytes",
    "other_bytes",
)


def _safe_divide(numerator: float, denominator: float) -> float | None:
    """Divides numerator by denominator, returning None if denominator is 0."""
    if denominator == 0:
        return None
    else:
        return numerator / denominator


def _cumulative_metrics_value(
    values: list[tuple[trace_time.TimePoint, int | float]]
) -> tuple[int | float, float | None]:
    """Returns the change and the rate for the specified cumulative metric."""
    (t0, v0), (t1, v1) = values[0], values[-1]
    return (v1 - v0, _safe_divide(v1 - v0, (t1 - t0).to_nanoseconds()))


def _cumulative_metrics_json(
    values: list[tuple[trace_time.TimePoint, int | float]]
) -> metrics.JSON:
    """Returns a JSON object holding the change and the rate for the specified cumulative metric."""
    (delta, rate) = _cumulative_metrics_value(values)
    return {
        "Delta": delta,
        "Rate": rate,
    }


def _gauges_metrics_values(
    values: list[tuple[trace_time.TimePoint, int | float]]
) -> list[int | float]:
    """Returns a JSON object holding the standard metric value keyed by metric name."""
    return list(v[1] for v in values)


def _gauges_metrics_json(
    values: list[tuple[trace_time.TimePoint, int | float]]
) -> metrics.JSON:
    """Returns a JSON object holding the standard metric value keyed by metric name."""
    results = trace_utils.standard_metrics_set(
        values=_gauges_metrics_values(values),
        label_prefix="",
        unit=metrics.Unit.bytes,
    )
    return {result.label: result.values[0] for result in results}


def _compute_management_time_ms(model: trace_model.Model) -> float:
    """Computes total unnormalized CPU time (ms) spent in memory management threads."""
    management_tids = {
        t.tid
        for p in model.processes
        for t in p.threads
        if t.name in _MEMORY_MANAGEMENT_THREAD_NAMES
    }
    if not management_tids or not model.scheduling_records:
        return 0.0

    entries: list[tuple[int, float, float]] = []
    for cpu_idx, records in model.scheduling_records.items():
        cs_records = [
            r for r in records if isinstance(r, trace_model.ContextSwitch)
        ]
        if len(cs_records) < 2:
            continue
        cs_records.sort(key=lambda r: r.start.to_epoch_nanoseconds())
        total_cpu_dur = (
            float(cs_records[-1].start.to_epoch_nanoseconds())
            - float(cs_records[0].start.to_epoch_nanoseconds())
        ) / _NS_PER_MS
        tid_durations: dict[int, float] = {}
        for i in range(len(cs_records) - 1):
            prev_record = cs_records[i]
            curr_record = cs_records[i + 1]
            out_tid = curr_record.outgoing_tid
            if (
                prev_record.tid == out_tid
                and out_tid in management_tids
                and not prev_record.is_idle()
            ):
                dur = (
                    float(curr_record.start.to_epoch_nanoseconds())
                    - float(prev_record.start.to_epoch_nanoseconds())
                ) / _NS_PER_MS
                tid_durations[out_tid] = tid_durations.get(out_tid, 0.0) + dur
        for dur in tid_durations.values():
            pct = dur / total_cpu_dur * 100.0 if total_cpu_dur > 0 else 0.0
            entries.append((cpu_idx, pct, dur))

    entries.sort(key=lambda item: (item[0], item[1]), reverse=True)
    return sum(dur for _, _, dur in entries)


class MemoryMetricsProcessor(trace_metrics.MetricsProcessor):
    """Computes statistics for values published by zircon kernel.

    Returns both freeform metrics and structured metrics.

    Freeform metrics is a JSON object with nested structure with the following path:

    "kernel" / field name / statistic label / float value

    Where:
        kernel: constant "kernel"
        field name: name of a field from `zx_info_kmem_stats_extended` and
            `zx_info_kmem_stats_compression` structures.
        statistic name: label of the statistic published by `trace_utils.standard_metrics_set`.
        float value: metric value.

    Sample:

    {
        "kernel": {
            "total_bytes": {
                "Min": 112
                "P5": 130
                [...]
            } ,
            [...]
        }
    }
    """

    @property
    @typing.override
    def event_patterns(self) -> set[str]:
        """Patterns describing the trace events needed to generate these metrics."""
        return set(_KERNEL_EVENT_NAMES)

    FREEFORM_METRICS_FILENAME = "memory"

    @typing.override
    def process_freeform_metrics(
        self, model: trace_model.Model
    ) -> tuple[str, metrics.JSON]:
        series_by_name = collections.defaultdict(list)
        for event in trace_utils.filter_events(
            model.all_events(),
            name=_KERNEL_EVENT_NAMES_SET,
            type=trace_model.CounterEvent,
        ):
            for name, value in event.args.items():
                series_by_name[name].append((event.start, value))

        return (
            self.FREEFORM_METRICS_FILENAME,
            dict(
                kernel={
                    name: _cumulative_metrics_json(series)
                    if name in _CUMULATIVE_METRIC_NAMES
                    else _gauges_metrics_json(series)
                    for name, series in series_by_name.items()
                }
            ),
        )

    @typing.override
    def process_metrics(
        self, model: trace_model.Model
    ) -> Collection[metrics.TestCaseResult]:
        series_by_name = collections.defaultdict(list)
        read_durations: list[int | float] = []
        dirty_durations: list[int | float] = []
        private_memory_series = []

        for process in model.processes:
            for thread in process.threads:
                for event in thread.events:
                    if (
                        isinstance(event, trace_model.CounterEvent)
                        and event.name in _KERNEL_EVENT_NAMES_SET
                    ):
                        for name, value in event.args.items():
                            if name in _STRUCTURED_METRIC_NAMES:
                                series_by_name[name].append(
                                    (event.start, value)
                                )
                        if event.name == _KERNEL_EVENT_BASIC and all(
                            field in event.args
                            for field in _KERNEL_PRIVATE_MEMORY_FIELDS
                        ):
                            private_memory_series.append(
                                (
                                    event.start,
                                    sum(
                                        event.args[field]
                                        for field in _KERNEL_PRIVATE_MEMORY_FIELDS
                                    ),
                                )
                            )
                    elif (
                        isinstance(event, trace_model.DurationEvent)
                        and event.name == "page_request_wait"
                        and event.duration is not None
                        and event.args.get("pager_name")
                    ):
                        req_type = event.args.get("type")
                        if req_type == "Read":
                            read_durations.append(
                                event.duration.to_nanoseconds()
                            )
                        elif req_type == "Dirty":
                            dirty_durations.append(
                                event.duration.to_nanoseconds()
                            )

        results = []
        if private_memory_series:
            results.extend(
                trace_utils.standard_metrics_set(
                    values=_gauges_metrics_values(private_memory_series),
                    label_prefix="Memory/Kernel/TotalPrivate",
                    unit=metrics.Unit.bytes,
                    doc_prefix=(
                        "Total bytes used by Kernel "
                        f"({', '.join(_KERNEL_PRIVATE_MEMORY_FIELDS)})"
                    ),
                )
            )

        for name, series in series_by_name.items():
            if name in _CUMULATIVE_METRIC_NAMES:
                values = [_cumulative_metrics_value(series)[0]]
            else:
                values = _gauges_metrics_values(series)
            results.append(
                metrics.TestCaseResult(
                    label=_STRUCTURED_METRIC_NAMES[name].structured_name,
                    values=values,
                    unit=_STRUCTURED_METRIC_NAMES[name].unit,
                    direction=_STRUCTURED_METRIC_NAMES[name].direction,
                )
            )

        total_management_time = _compute_management_time_ms(model)
        results.append(
            metrics.TestCaseResult(
                label=_MANAGEMENT_STRUCTURED_METRIC_NAME.structured_name,
                values=(total_management_time,),
                unit=_MANAGEMENT_STRUCTURED_METRIC_NAME.unit,
                direction=metrics.Direction.smallerIsBetter,
            )
        )

        for durations, metric_key in (
            (read_durations, "page_request_wait_read"),
            (dirty_durations, "page_request_wait_dirty"),
        ):
            metric_info = _STRUCTURED_METRIC_NAMES[metric_key]
            if durations:
                results.append(
                    metrics.TestCaseResult(
                        label=metric_info.structured_name,
                        values=durations,
                        unit=metric_info.unit,
                        direction=metric_info.direction,
                    )
                )

        return results
