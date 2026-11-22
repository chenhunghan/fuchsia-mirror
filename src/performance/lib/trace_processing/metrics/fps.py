#!/usr/bin/env fuchsia-vendored-python
# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""FPS trace metrics."""

import logging
import statistics
import typing
from collections.abc import MutableSequence

from reporting import metrics
from tp_shell import PerfettoTraceProcessor
from trace_processing import trace_metrics, trace_model, trace_utils
from trace_processing.metrics import _gfx_sql

_LOGGER: logging.Logger = logging.getLogger("FPSMetricsProcessor")
_EVENT_CATEGORY = _gfx_sql.EVENT_CATEGORY
_SCENIC_RENDER_EVENT_NAME: str = "RenderFrame"

# Skip early events caused by a trace start race across Scenic, Magma, and
# Display (https://fxbug.dev/322849857).
_RENDER_EVENTS_QUERY: str = f"""\
{_gfx_sql.build_edges_table_sql("_fps_edges")}

CREATE OR REPLACE PERFETTO TABLE _fps_render_events AS
WITH {_gfx_sql.build_common_start_ctes_sql("_fps_edges", _SCENIC_RENDER_EVENT_NAME)}
SELECT id, ts
FROM slice
WHERE category = '{_EVENT_CATEGORY}'
  AND name = '{_SCENIC_RENDER_EVENT_NAME}'
  AND dur > 0
  AND ts >= (SELECT start_ts FROM _common_start);

SELECT id, ts
FROM _fps_render_events
ORDER BY ts ASC, id ASC;
"""

_FLOW_VSYNC_QUERY: str = _gfx_sql.build_flow_vsync_query(
    edges_table="_fps_edges",
    events_table="_fps_render_events",
    event_id_col="render_id",
    min_ts_col="ts",
)


class FpsMetricsProcessor(trace_metrics.MetricsProcessor):
    """Computes FPS (Frames-per-Second) metrics.

    Calculates Scenic's frames-per-second by measuring the window between consecutive vsyncs that
    are triggered by Scenic's frame-rendering code. Flow events in the trace enable this class to
    reliably correlate the correct events to calculate this duration.

    By default, this module reports aggregate latency measurements -- such as min, max, average, and
    percentiles -- calculated across all frames rendered during the test. It can be
    configured to instead report a time series of measurements, one for each event.
    """

    def __init__(self, aggregates_only: bool = True):
        """Constructor.

        Args:
            aggregates_only: When True, generates FpsMin, FpsMax, FpsAverage and FpsP* (percentiles).
                Otherwise generates Fps metric with all Fps values.
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
            render_rows = tp.run_query(_RENDER_EVENTS_QUERY)
            if not render_rows:
                _LOGGER.warning(
                    "No '%s' slices in category '%s' found after the common "
                    "process start time; cannot compute fps",
                    _SCENIC_RENDER_EVENT_NAME,
                    _EVENT_CATEGORY,
                )
                return []

            flow_rows = tp.run_query(_FLOW_VSYNC_QUERY)

        vsync_by_render_id: dict[int, int] = {
            int(row["render_id"]): int(row["vsync_ts"]) for row in flow_rows
        }
        vsync_timestamps: list[int] = [
            vsync_by_render_id[render_id]
            for row in render_rows
            if (render_id := int(row["id"])) in vsync_by_render_id
        ]

        _LOGGER.debug(f"{len(render_rows)} cpu_render_start_events")
        if len(vsync_timestamps) < 2:
            _LOGGER.warning(
                "Fewer than two vsync events are present. Perhaps the trace "
                "duration is too short to provide fps information"
            )
            return []

        fps_values = _gfx_sql.compute_fps_values(vsync_timestamps)

        if len(fps_values) == 0:
            _LOGGER.warning("Not enough valid vsyncs")
            return []

        fps_mean: float = statistics.mean(fps_values)
        _LOGGER.info(f"Average FPS: {fps_mean}")

        if self.aggregates_only:
            return trace_utils.standard_metrics_set(
                values=fps_values,
                label_prefix="Fps",
                unit=metrics.Unit.framesPerSecond,
            )
        return [
            metrics.TestCaseResult(
                "Fps", metrics.Unit.framesPerSecond, fps_values
            )
        ]
