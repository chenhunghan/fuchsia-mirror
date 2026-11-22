#!/usr/bin/env fuchsia-vendored-python
# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import itertools
import logging
import typing
from collections.abc import MutableSequence

from reporting import metrics
from tp_shell import PerfettoTraceProcessor
from trace_processing import trace_metrics, trace_model, trace_utils

_LOGGER: logging.Logger = logging.getLogger(__name__)
_GPU_USAGE_EVENT_NAME = "GPU Utilization"
_GPU_UTILIZATION_ARG_NAME = "utilization"

# Query counter samples for GPU Utilization ordered chronologically.
# FXT counter records are named "<event_name>:<arg_name>:<counter_id>" in
# Perfetto's `counter_track` table (see `fuchsia_trace_parser.cc`), whereas
# Chrome-JSON counter records are named "<event_name> <arg_name>" (or
# "<event_name> id: <id> <arg_name>").
_GPU_USAGE_QUERY: str = f"""\
SELECT
  c.ts AS ts,
  c.value AS value
FROM counter AS c
JOIN counter_track AS ct ON c.track_id = ct.id
WHERE ct.name GLOB '{_GPU_USAGE_EVENT_NAME}:{_GPU_UTILIZATION_ARG_NAME}:*'
   OR ct.name = '{_GPU_USAGE_EVENT_NAME}'
   OR ct.name = '{_GPU_USAGE_EVENT_NAME} {_GPU_UTILIZATION_ARG_NAME}'
   OR ct.name GLOB '{_GPU_USAGE_EVENT_NAME} id: * {_GPU_UTILIZATION_ARG_NAME}'
ORDER BY c.ts ASC, c.id ASC;
"""


class GpuMetricsProcessor(trace_metrics.MetricsProcessor):
    """Computes GPU utilization metrics.

    GPU utilization metrics mark the percentage of time the GPU is busy performing
    work. Trace counters record utilization as a fraction from 0 to 1, which this
    processor converts to a percentage from 0 to 100.
    """

    def __init__(
        self,
        aggregates_only: bool = True,
    ):
        """Constructor.

        Args:
            aggregates_only: When True, generates GpuMin, GpuMax, GpuAverage and GpuP* (%iles).
                Otherwise generates GpuUtilization metric with all gpu values.
        """
        self.aggregates_only: bool = aggregates_only

    @property
    @typing.override
    def event_patterns(self) -> set[str]:
        """This processor uses Perfetto SQL on the FXT trace directly."""
        return set()

    @property
    @typing.override
    def category_names(self) -> set[str]:
        """This processor uses Perfetto SQL on the FXT trace directly."""
        return set()

    @typing.override
    def process_metrics(
        self, model: trace_model.Model
    ) -> MutableSequence[metrics.TestCaseResult]:
        """Unsupported; use `process_metrics_with_fxt()` instead."""
        return []

    @typing.override
    def process_metrics_with_fxt(
        self, fxt_path: str
    ) -> MutableSequence[metrics.TestCaseResult]:
        with PerfettoTraceProcessor(trace_path=fxt_path) as tp:
            rows = tp.run_query(_GPU_USAGE_QUERY)

        if not rows:
            _LOGGER.info(
                "No gpu usage measurements are present. Perhaps the trace "
                "duration is too short to provide gpu usage information"
            )
            return []

        gpu_percentages: list[float] = [
            float(row["value"]) * 100.0 for row in rows
        ]

        if self.aggregates_only:
            # The duration for each utilization is the time between it and the next one.
            # We don't know the duration for the last event so add one '0'.
            gpu_durations: list[float] = [
                float(int(curr["ts"]) - int(prev["ts"]))
                for prev, curr in itertools.pairwise(rows)
            ] + [0.0]
            # Guard against sum(gpu_durations) == 0 (e.g. a single-sample trace whose
            # trailing duration is 0.0), as standard_metrics_set divides by sum(durations)
            # and would raise ZeroDivisionError.
            return trace_utils.standard_metrics_set(
                values=gpu_percentages,
                label_prefix="Gpu",
                unit=metrics.Unit.percent,
                durations=gpu_durations if sum(gpu_durations) > 0 else None,
            )
        return [
            metrics.TestCaseResult(
                "GpuUtilization", metrics.Unit.percent, gpu_percentages
            )
        ]
