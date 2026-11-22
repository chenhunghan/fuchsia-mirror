#!/usr/bin/env fuchsia-vendored-python
# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Per App Present trace metrics."""

import bisect
import logging
import statistics
import typing
from collections.abc import MutableSequence

from reporting import metrics
from tp_shell import PerfettoTraceProcessor
from trace_processing import trace_metrics, trace_model, trace_time, trace_utils
from trace_processing.metrics import _gfx_sql

_LOGGER: logging.Logger = logging.getLogger("AppRenderLatencyMetricsProcessor")
_EVENT_CATEGORY = _gfx_sql.EVENT_CATEGORY
_PRESENT_EVENT_NAME: str = "Flatland::PerAppPresent[{}]"
# Legacy flatland-rainbow traces (before FlatlandApp::PresentFlowBegin was
# added) emit flow_begin after draw inside this slice; anchor them at the end of
# their last gfx child instead of the slice start.
_LEGACY_POST_DRAW_PARENT_SLICE: str = "FlatlandApp::PresentBegin"


def _build_present_events_query(debug_name: str) -> str:
    """Builds the setup query to materialize `_app_render_edges` and `_app_render_present_events`."""
    raw_present_event_name = _PRESENT_EVENT_NAME.format(debug_name)
    present_event_name = _gfx_sql.escape_sql_literal(raw_present_event_name)
    edges_sql = _gfx_sql.build_edges_table_sql("_app_render_edges")
    # Skip early events caused by a trace start race across Scenic, Magma, and
    # Display (https://fxbug.dev/322849857).
    common_start_ctes = _gfx_sql.build_common_start_ctes_sql(
        "_app_render_edges", raw_present_event_name
    )
    return f"""\
{edges_sql}

CREATE OR REPLACE PERFETTO TABLE _app_render_present_events AS
WITH {common_start_ctes},
_present_slices AS (
  SELECT
    s.id AS id,
    s.ts AS slice_ts
  FROM slice AS s
  CROSS JOIN _common_start AS cs
  WHERE s.category = '{_EVENT_CATEGORY}'
    AND s.name = '{present_event_name}'
    AND s.dur > 0
    AND s.ts >= cs.start_ts
),
_present_sources AS (
  SELECT
    ps.id AS id,
    ps.slice_ts AS slice_ts,
    cs.start_ts AS cs_start_ts,
    src.id AS src_id,
    src.ts AS src_ts,
    src.name AS src_name
  FROM _present_slices AS ps
  CROSS JOIN _common_start AS cs
  LEFT JOIN flow AS f ON f.slice_in = ps.id
  LEFT JOIN slice AS src
    ON src.id = f.slice_out
    AND src.category = '{_EVENT_CATEGORY}'
),
_src_max_child_end AS (
  SELECT
    c.parent_id AS src_id,
    MAX(c.ts + c.dur) AS max_child_end
  FROM slice AS c
  WHERE c.parent_id IN (
    SELECT DISTINCT src_id
    FROM _present_sources
    WHERE src_id IS NOT NULL
      AND src_name = '{_LEGACY_POST_DRAW_PARENT_SLICE}'
  )
    AND c.category = '{_EVENT_CATEGORY}'
    AND c.dur >= 0
  GROUP BY c.parent_id
)
-- Perfetto binds flows to their enclosing slice, so `start_ts` is the start of
-- the slice enclosing `flow_begin` (`src.ts`). Apps should wrap `flow_begin` in
-- a dedicated post-draw leaf duration (e.g. `FlatlandApp::PresentFlowBegin`).
-- Only for legacy `{_LEGACY_POST_DRAW_PARENT_SLICE}` sources (pre-
-- `PresentFlowBegin` `flatland-rainbow` traces that emit `flow_begin` after
-- `FlatlandViewProvider::draw` inside `PresentBegin`), resolve `start_ts` to
-- `MAX(c.ts + c.dur)` over `gfx` children so `app.draw()` time is excluded.
-- Uses `_present_sources` + `_src_max_child_end` CTEs instead of a correlated
-- scalar subquery to avoid O(P * |flow|) and per-present `slice` scans.
SELECT
  p.id AS id,
  p.slice_ts AS slice_ts,
  COALESCE(
    MIN(
      CASE
        WHEN p.src_id IS NOT NULL THEN COALESCE(ce.max_child_end, p.src_ts)
        ELSE NULL
      END
    ),
    p.slice_ts
  ) AS start_ts
FROM _present_sources AS p
LEFT JOIN _src_max_child_end AS ce ON ce.src_id = p.src_id
GROUP BY p.id, p.slice_ts
HAVING MIN(p.src_ts) IS NULL OR MIN(p.src_ts) >= MAX(p.cs_start_ts);

SELECT id, start_ts
FROM _app_render_present_events
ORDER BY slice_ts ASC, id ASC;
"""


_FLOW_VSYNC_QUERY: str = _gfx_sql.build_flow_vsync_query(
    edges_table="_app_render_edges",
    events_table="_app_render_present_events",
    event_id_col="present_id",
    min_ts_col="start_ts",
)


class AppRenderLatencyMetricsProcessor(trace_metrics.MetricsProcessor):
    """Computes present-latency metrics for a given app.

    App render latency is measured from the moment that the chosen app begins presenting through
    the Flatland API until the first vsync that corresponds with app content being displayed on
    screen. Flow events in the trace enable this class to reliably correlate the correct events to
    calculate this duration.

    Because Perfetto binds `flow_begin` events to their enclosing slice, the latency anchor is the
    start timestamp of the `gfx` slice enclosing `flow_begin` (with a legacy exception for
    `FlatlandApp::PresentBegin`, which anchors at the end of its last `gfx` child). Apps should
    wrap `flow_begin` in a dedicated post-draw leaf duration slice (as `flatland-rainbow` does with
    `FlatlandApp::PresentFlowBegin`) so earlier draw work is excluded from present latency.

    By default, this module reports aggregate latency measurements -- such as min, max, average, and
    percentiles -- calculated across all presentations of the chosen app during the test. It can be
    configured to instead report a time series of latency measurements, one for each event.
    """

    def __init__(self, debug_name: str, aggregates_only: bool = True):
        """Constructor.

        Args:
            debug_name: the flatland client debug name.
            aggregates_only: When True, generates AppRenderVsyncLatencyMin,
                AppRenderVsyncLatencyMax, AppRenderVsyncLatencyAverage and
                AppRenderVsyncLatencyP* (percentiles).
                Otherwise generates AppRenderVsyncLatency metric with all
                AppRenderVsyncLatency values.
        """
        self._debug_name: str = debug_name
        self._aggregates_only: bool = aggregates_only

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
        present_query = _build_present_events_query(self._debug_name)
        with PerfettoTraceProcessor(trace_path=fxt_path) as tp:
            present_rows = tp.run_query(present_query)
            if not present_rows:
                _LOGGER.warning("No valid present events found in trace.")
                return []

            flow_rows = tp.run_query(_FLOW_VSYNC_QUERY)
            flow_vsync_by_id: dict[int, int] = {
                int(row["present_id"]): int(row["vsync_ts"])
                for row in flow_rows
            }

            fallback_vsync_ts: list[int] = []
            if len(flow_vsync_by_id) < len(present_rows):
                fallback_rows = tp.run_query(_gfx_sql.FALLBACK_VSYNC_QUERY)
                fallback_vsync_ts = [int(row["ts"]) for row in fallback_rows]

        present_latencies: list[float] = []
        vsync_timestamps: list[int] = []

        for row in present_rows:
            present_id = int(row["id"])
            start_ts = int(row["start_ts"])
            vsync_ts = flow_vsync_by_id.get(present_id)

            # If vsync_ts is None, it could be that a frame was dropped or not
            # rendered; find the next vsync in chronological order.
            if vsync_ts is None and fallback_vsync_ts:
                idx = bisect.bisect_left(fallback_vsync_ts, start_ts)
                if idx < len(fallback_vsync_ts):
                    vsync_ts = fallback_vsync_ts[idx]

            # If there isn't a vsync, loop through and find the next event.
            if vsync_ts is None:
                continue

            latency_ms = trace_time.TimeDelta(
                vsync_ts - start_ts
            ).to_milliseconds_f()
            present_latencies.append(round(latency_ms, 3))
            vsync_timestamps.append(vsync_ts)

        if len(vsync_timestamps) == 0:
            _LOGGER.warning("Not enough valid vsyncs")
            return []

        present_latency_mean: float = statistics.mean(present_latencies)
        _LOGGER.info(f"Average Present Latency: {present_latency_mean}")

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

        if self._aggregates_only:
            return trace_utils.standard_metrics_set(
                values=present_latencies,
                label_prefix="AppRenderVsyncLatency",
                unit=metrics.Unit.milliseconds,
            ) + trace_utils.standard_metrics_set(
                values=fps_values,
                label_prefix="AppFps",
                unit=metrics.Unit.framesPerSecond,
            )
        return [
            metrics.TestCaseResult(
                "AppRenderVsyncLatency",
                metrics.Unit.milliseconds,
                present_latencies,
            ),
            metrics.TestCaseResult(
                "AppFps", metrics.Unit.framesPerSecond, fps_values
            ),
        ]
