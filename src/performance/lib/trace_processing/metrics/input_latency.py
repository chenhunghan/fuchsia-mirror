#!/usr/bin/env fuchsia-vendored-python
# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""input trace metrics."""

import bisect
import logging
import statistics
import typing
from collections.abc import MutableSequence

from reporting import metrics
from tp_shell import PerfettoTraceProcessor
from trace_processing import trace_metrics, trace_model, trace_time, trace_utils

_LOGGER: logging.Logger = logging.getLogger("InputLatencyMetricsProcessor")
_CATEGORY_INPUT: str = "input"
_INPUT_EVENT_NAME: str = "input-device-process-reports"
_POINTERINJECTOR_REGISTER_EVENT_NAME: str = "PointerinjectorRegistry::Register"
_CATEGORY_GFX: str = "gfx"
_DISPLAY_VSYNC_EVENT_NAME: str = "Flatland::DisplayCompositor::OnVsync"

# Queries run in one PerfettoTraceProcessor session so the trace is parsed once:
# 1. _INPUT_EVENTS_QUERY: finds the last Register slice end time and materializes
#    filtered input slices into `_input_latency_events`.
# 2. _FLOW_VSYNC_QUERY: depends on `_input_latency_events` created above. Builds
#    the reachability graph from `flow` and `slice.parent_id` edges restricted
#    to `{_CATEGORY_GFX}` and `{_CATEGORY_INPUT}` slices on both endpoints
#    (mirroring the legacy `trace2json --category` filter and preventing
#    traversal through unrelated slices such as kernel IPC `ChannelMessage`/
#    `ChannelFlow` events). Makes complete (`dur > 0`) OnVsync slices terminal
#    by excluding outgoing edges from them so DFS never continues into later
#    frames (an intentional divergence from legacy rather than strictly
#    semantics-preserving, because `dur = 0` child -> parent edges can step
#    backward in time to an enclosing slice). Only OnVsync slices starting at
#    or after the input event (`s.ts >= ie.ts`) are selected, so such backward
#    steps can never pick an OnVsync that precedes the input event. Uses
#    `graph_reachable_weight_bounded_dfs!` solely because it reports
#    `root_node_id`; weight 0 / bound 1 intentionally leaves the search
#    unbounded by weight (do not use edge weights to bound, as the macro breaks
#    out of the root's whole DFS in visit order rather than timestamp order).
# 3. _FALLBACK_VSYNC_QUERY: runs only when any input slice lacks a flow-matched
#    OnVsync, returning ordered OnVsync timestamps for chronological matching.
#
# Duration filter choices:
# - Register, input events, and flow-matched OnVsync (both target selection and
#   terminal exclusion) use `dur > 0` to select complete DurationEvent slices
#   (excluding dur = 0 instants and dur = -1 unterminated slices), matching
#   legacy DurationEvent / end_time() filters.
# - The fallback query uses `dur >= 0` to match legacy's `trace_model.Event`
#   fallback, accepting both DurationEvents and dur = 0 InstantEvents while
#   still excluding dur = -1 unterminated slices.
_INPUT_EVENTS_QUERY: str = f"""\
CREATE OR REPLACE PERFETTO TABLE _input_latency_events AS
WITH _last_register AS (
  SELECT MAX(ts + dur) AS end_ts
  FROM slice
  WHERE category = '{_CATEGORY_INPUT}'
    AND name = '{_POINTERINJECTOR_REGISTER_EVENT_NAME}'
    AND dur > 0
)
SELECT id, ts
FROM slice
WHERE category = '{_CATEGORY_INPUT}'
  AND name = '{_INPUT_EVENT_NAME}'
  AND dur > 0
  AND (
    (SELECT end_ts FROM _last_register) IS NULL
    OR ts >= (SELECT end_ts FROM _last_register)
  );

SELECT id, ts
FROM _input_latency_events
ORDER BY ts ASC, id ASC;
"""

_FLOW_VSYNC_QUERY: str = f"""\
INCLUDE PERFETTO MODULE graphs.search;

CREATE OR REPLACE PERFETTO TABLE _input_latency_edges AS
-- Restrict all edges to '{_CATEGORY_GFX}' and '{_CATEGORY_INPUT}' slices on
-- both endpoints. This mirrors the legacy `trace2json --category` filter and
-- prevents traversal through kernel IPC (`ChannelMessage`/`ChannelFlow`) or
-- other out-of-scope slices.
--
-- Flow edges, excluding flows leaving terminal (dur > 0) OnVsync slices.
SELECT
  f.slice_out AS source_node_id,
  f.slice_in AS dest_node_id,
  0 AS edge_weight
FROM flow AS f
JOIN slice AS src ON src.id = f.slice_out
JOIN slice AS dst ON dst.id = f.slice_in
WHERE f.slice_out IS NOT NULL
  AND f.slice_in IS NOT NULL
  AND src.category IN ('{_CATEGORY_GFX}', '{_CATEGORY_INPUT}')
  AND dst.category IN ('{_CATEGORY_GFX}', '{_CATEGORY_INPUT}')
  AND NOT (
    src.category IS '{_CATEGORY_GFX}'
    AND src.name IS '{_DISPLAY_VSYNC_EVENT_NAME}'
    AND src.dur > 0
  )
UNION ALL
-- Enclosing -> child edges, excluding children of terminal (dur > 0) OnVsync slices.
SELECT
  c.parent_id AS source_node_id,
  c.id AS dest_node_id,
  0 AS edge_weight
FROM slice AS c
JOIN slice AS p ON p.id = c.parent_id
WHERE c.parent_id IS NOT NULL
  AND c.category IN ('{_CATEGORY_GFX}', '{_CATEGORY_INPUT}')
  AND p.category IN ('{_CATEGORY_GFX}', '{_CATEGORY_INPUT}')
  AND NOT (
    p.category IS '{_CATEGORY_GFX}'
    AND p.name IS '{_DISPLAY_VSYNC_EVENT_NAME}'
    AND p.dur > 0
  )
UNION ALL
-- Instant child (dur = 0) -> enclosing duration: Perfetto binds FXT flow
-- events to the topmost track slice, which can be an instant slice (dur = 0),
-- whereas the legacy trace model bound those flows to the enclosing duration.
-- Instant slices (dur = 0) cannot be dur > 0 terminal OnVsyncs.
SELECT
  c.id AS source_node_id,
  c.parent_id AS dest_node_id,
  0 AS edge_weight
FROM slice AS c
JOIN slice AS p ON p.id = c.parent_id
WHERE c.dur = 0
  AND c.parent_id IS NOT NULL
  AND c.category IN ('{_CATEGORY_GFX}', '{_CATEGORY_INPUT}')
  AND p.category IN ('{_CATEGORY_GFX}', '{_CATEGORY_INPUT}');

SELECT
  r.root_node_id AS input_id,
  MIN(s.ts) AS vsync_ts
FROM graph_reachable_weight_bounded_dfs!(
  _input_latency_edges,
  (SELECT id AS root_node_id, 1 AS root_target_weight FROM _input_latency_events),
  1
) AS r
JOIN _input_latency_events AS ie ON ie.id = r.root_node_id
JOIN slice AS s ON s.id = r.node_id
WHERE s.category = '{_CATEGORY_GFX}'
  AND s.name = '{_DISPLAY_VSYNC_EVENT_NAME}'
  AND s.dur > 0
  AND s.ts >= ie.ts
GROUP BY r.root_node_id;
"""

_FALLBACK_VSYNC_QUERY: str = f"""\
SELECT ts
FROM slice
WHERE category = '{_CATEGORY_GFX}'
  AND name = '{_DISPLAY_VSYNC_EVENT_NAME}'
  AND dur >= 0
ORDER BY ts ASC;
"""


class InputLatencyMetricsProcessor(trace_metrics.MetricsProcessor):
    """Measures the time it takes for an input event to result in user-visible change on-screen.

    Calculates the time needed for an input event to progress through the input pipeline and
    result in a change on the screen at vsync. Flow events in the trace enable this class to
    reliably correlate input events to the first user-visible output that results.

    By default, this module reports aggregate latency measurements -- such as min, max, average, and
    percentiles -- calculated across all input events generated during the test. It can be
    configured to instead report a time series of latency measurements, one for each input event.
    """

    def __init__(self, aggregates_only: bool = True):
        """Constructor.

        Args:
            aggregates_only: When True, generates InputLatencyMin,
                InputLatencyMax, InputLatencyAverage and
                InputLatencyP* (percentiles).
                Otherwise generates total_input_latency metric with all
                latency values.
        """
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
        with PerfettoTraceProcessor(trace_path=fxt_path) as tp:
            input_rows = tp.run_query(_INPUT_EVENTS_QUERY)
            if not input_rows:
                _LOGGER.warning("No valid input latency events found in trace.")
                return []

            flow_rows = tp.run_query(_FLOW_VSYNC_QUERY)
            flow_vsync_by_id: dict[int, int] = {
                int(row["input_id"]): int(row["vsync_ts"]) for row in flow_rows
            }

            fallback_vsync_ts: list[int] = []
            if len(flow_vsync_by_id) < len(input_rows):
                fallback_rows = tp.run_query(_FALLBACK_VSYNC_QUERY)
                fallback_vsync_ts = [int(row["ts"]) for row in fallback_rows]

        latencies: list[float] = []
        max_latency: float = -1.0
        max_latency_ts: int | None = None

        for row in input_rows:
            input_id = int(row["id"])
            input_ts = int(row["ts"])
            vsync_ts = flow_vsync_by_id.get(input_id)

            # If vsync_ts is None, it could be that a frame was dropped or not
            # rendered; find the next vsync in chronological order.
            if vsync_ts is None and fallback_vsync_ts:
                idx = bisect.bisect_left(fallback_vsync_ts, input_ts)
                if idx < len(fallback_vsync_ts):
                    vsync_ts = fallback_vsync_ts[idx]

            # No following vsync (flow or chronological); skip this event.
            if vsync_ts is None:
                continue

            latency_ms = trace_time.TimeDelta(
                vsync_ts - input_ts
            ).to_milliseconds_f()
            latencies.append(latency_ms)
            if latency_ms > max_latency:
                max_latency = latency_ms
                max_latency_ts = input_ts

        if not latencies:
            _LOGGER.warning("No valid input latency events found in trace.")
            return []

        if max_latency_ts is not None:
            _LOGGER.info(
                f"InputLatencyMax: {max_latency} ms at timestamp {trace_time.TimePoint(max_latency_ts)}"
            )

        latency_mean: float = statistics.mean(latencies)
        _LOGGER.info(f"Average Present Latency: {latency_mean}")

        if self._aggregates_only:
            return trace_utils.standard_metrics_set(
                values=latencies,
                label_prefix="InputLatency",
                unit=metrics.Unit.milliseconds,
            )
        return [
            metrics.TestCaseResult(
                "total_input_latency",
                metrics.Unit.milliseconds,
                latencies,
            ),
        ]
