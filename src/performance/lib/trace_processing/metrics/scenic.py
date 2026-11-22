#!/usr/bin/env fuchsia-vendored-python
# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Scenic trace metrics."""

import collections
import logging
import statistics
import typing
from collections.abc import Collection, MutableSequence, Sequence

from reporting import metrics
from tp_shell import PerfettoTraceProcessor
from trace_processing import trace_metrics, trace_model, trace_time, trace_utils
from trace_processing.metrics import _gfx_sql

_LOGGER: logging.Logger = logging.getLogger("ScenicMetricsProcessor")
_EVENT_CATEGORY = _gfx_sql.EVENT_CATEGORY
_SCENIC_START_EVENT_NAME: str = "ApplyScheduledSessionUpdates"
_SCENIC_RENDER_EVENT_NAME: str = "RenderFrame"
_DISPLAY_VSYNC_READY_EVENT_NAME = _gfx_sql.DISPLAY_VSYNC_EVENT_NAME
_FRAME_NUMBER_ARG_NAME: str = "frame_number"

# Skip early events caused by a trace start race across Scenic, Magma, and
# Display (https://fxbug.dev/322849857).
# Checks both `frame_number` and `args.frame_number` because FXT stores event
# arguments directly under `<arg_name>`, whereas Chrome-JSON traces imported
# into Perfetto prefix them under `args.<arg_name>`.
_START_EVENTS_QUERY: str = f"""\
{_gfx_sql.build_edges_table_sql("_scenic_edges")}

CREATE OR REPLACE PERFETTO TABLE _scenic_raw_starts AS
WITH {_gfx_sql.build_common_start_ctes_sql("_scenic_edges", _SCENIC_START_EVENT_NAME)}
SELECT
  s.id AS id,
  s.ts AS ts,
  COALESCE(
    EXTRACT_ARG(s.arg_set_id, '{_FRAME_NUMBER_ARG_NAME}'),
    EXTRACT_ARG(s.arg_set_id, 'args.{_FRAME_NUMBER_ARG_NAME}')
  ) AS frame_number
FROM slice AS s
WHERE s.category = '{_EVENT_CATEGORY}'
  AND s.name = '{_SCENIC_START_EVENT_NAME}'
  AND s.dur > 0
  AND s.ts >= (SELECT start_ts FROM _common_start);

-- Filter out squashed frames by keeping only the latest start event per
-- frame_number. If Scenic's frame scheduler applies updates and then decides
-- that no frame needs to be displayed, the frame number is NOT incremented;
-- keeping the latest timestamp per frame_number retains only the start event
-- corresponding to the rendered frame.
CREATE OR REPLACE PERFETTO TABLE _scenic_start_events AS
WITH _ranked AS (
  SELECT
    id,
    ts,
    ROW_NUMBER() OVER (
      PARTITION BY frame_number
      ORDER BY ts DESC, id DESC
    ) AS rn
  FROM _scenic_raw_starts
  WHERE frame_number IS NOT NULL
)
SELECT id, ts
FROM _ranked
WHERE rn = 1;

SELECT
  (
    SELECT ts
    FROM _scenic_raw_starts
    WHERE frame_number IS NULL
    ORDER BY ts ASC, id ASC
    LIMIT 1
  ) AS missing_frame_number_ts,
  (SELECT COUNT(*) FROM _scenic_start_events) AS start_count;
"""

# Both DFS steps use `graph_reachable_weight_bounded_dfs!` with `0 AS edge_weight`
# and bound `1` solely because it outputs `root_node_id` (see `_gfx_sql.py`).
_RENDER_VSYNC_QUERY: str = f"""\
CREATE OR REPLACE PERFETTO TABLE _scenic_start_to_render AS
WITH _candidates AS (
  SELECT
    r.root_node_id AS start_id,
    se.ts AS start_ts,
    rf.id AS render_id,
    rf.ts AS render_ts,
    rf.ts + rf.dur AS render_end_ts,
    tt.utid AS render_utid,
    ROW_NUMBER() OVER (
      PARTITION BY r.root_node_id
      ORDER BY rf.ts ASC, rf.id ASC
    ) AS rn
  FROM graph_reachable_weight_bounded_dfs!(
    _scenic_edges,
    (SELECT id AS root_node_id, 1 AS root_target_weight FROM _scenic_start_events),
    1
  ) AS r
  JOIN _scenic_start_events AS se ON se.id = r.root_node_id
  JOIN slice AS rf ON rf.id = r.node_id
  LEFT JOIN thread_track AS tt ON rf.track_id = tt.id
  WHERE rf.category = '{_EVENT_CATEGORY}'
    AND rf.name = '{_SCENIC_RENDER_EVENT_NAME}'
    AND rf.dur > 0
    AND rf.ts >= se.ts
)
SELECT start_id, start_ts, render_id, render_ts, render_end_ts, render_utid
FROM _candidates
WHERE rn = 1;

WITH _render_to_vsync AS (
  SELECT
    r.root_node_id AS render_id,
    MIN(v.ts) AS vsync_ts
  FROM graph_reachable_weight_bounded_dfs!(
    _scenic_edges,
    (
      SELECT DISTINCT render_id AS root_node_id, 1 AS root_target_weight
      FROM _scenic_start_to_render
    ),
    1
  ) AS r
  JOIN _scenic_start_to_render AS sr ON sr.render_id = r.root_node_id
  JOIN slice AS v ON v.id = r.node_id
  WHERE v.category = '{_EVENT_CATEGORY}'
    AND v.name = '{_DISPLAY_VSYNC_READY_EVENT_NAME}'
    AND v.dur > 0
    AND v.ts >= sr.render_ts
  GROUP BY r.root_node_id
)
SELECT
  sr.start_ts,
  sr.render_end_ts,
  sr.render_utid,
  rv.vsync_ts
FROM _scenic_start_to_render AS sr
JOIN _render_to_vsync AS rv ON rv.render_id = sr.render_id
ORDER BY sr.start_ts ASC, sr.start_id ASC;
"""

_CHECK_SCHED_EXISTS_QUERY: str = "SELECT 1 FROM sched LIMIT 1;"


def _build_sched_query(utids: Collection[int]) -> str:
    utid_list = ", ".join(str(u) for u in utids)
    return f"""\
SELECT utid, ts, dur
FROM sched
WHERE utid IN ({utid_list})
ORDER BY ts ASC, id ASC;
"""


def _compute_window_overlaps(
    segments: Collection[tuple[int, int]],
    windows: Sequence[tuple[int, int]],
) -> list[int]:
    """Merges overlapping `[start, end)` segments and computes overlap per sorted window."""
    if not windows:
        return []
    if not segments:
        return [0] * len(windows)

    sorted_segments = sorted(segments, key=lambda s: s[0])
    # Merge overlapping segments to prevent double-counting from trace anomalies
    # (e.g. dropped events).
    merged_segments = [sorted_segments[0]]
    for start, end in sorted_segments[1:]:
        prev_start, prev_end = merged_segments[-1]
        if start <= prev_end:
            merged_segments[-1] = (prev_start, max(prev_end, end))
        else:
            merged_segments.append((start, end))

    results = [0] * len(windows)
    first_active_seg_idx = 0
    for idx, (w_start, w_end) in enumerate(windows):
        # Advance past segments that end before the start of this window.
        while (
            first_active_seg_idx < len(merged_segments)
            and merged_segments[first_active_seg_idx][1] <= w_start
        ):
            first_active_seg_idx += 1

        curr_seg_idx = first_active_seg_idx
        while curr_seg_idx < len(merged_segments):
            s_start, s_end = merged_segments[curr_seg_idx]
            # Segments are sorted; any later segment also starts after w_end.
            if s_start >= w_end:
                break
            overlap_start = max(s_start, w_start)
            overlap_end = min(s_end, w_end)
            if overlap_end > overlap_start:
                results[idx] += overlap_end - overlap_start
            curr_seg_idx += 1

    return results


class ScenicMetricsProcessor(trace_metrics.MetricsProcessor):
    """Computes CPU and GPU time spent rendering frames in Scenic.

    Calculates total time-to-render-frame by measuring from the moment that Scenic reports it has
    begun computing frame contents to the moment that is ready for vsync. Also tracks time spent on
    CPU-bound operations for each frame. Flow events in the trace enable this class to reliably
    correlate the correct events to calculate this duration.

    By default, this module reports aggregate latency measurements -- such as min, max, average, and
    percentiles -- calculated across all frames rendered during the test. It can be
    configured to instead report a time series of measurements, one for each event.
    """

    def __init__(
        self, aggregates_only: bool = True, include_render_total: bool = True
    ):
        """Constructor.

        Args:
            aggregates_only: When True, generates RenderCpu[Min|Max|Average|P*] and
                RenderTotal[Min|Max|Average|P*].
                Otherwise generates RenderCpu and RenderTotal with the raw values.
            include_render_total: When True, collects RenderTotal metrics.
        """
        self.aggregates_only: bool = aggregates_only
        self.include_render_total: bool = include_render_total

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
        """Generates RenderCpu and RenderTotal metrics from an FXT trace.

        Returns:
            Computed render metrics, or `[]` (with a warning log) when no
            `ApplyScheduledSessionUpdates` start events or matched
            `RenderFrame` -> `OnVsync` frames are present.

        Raises:
            ValueError: If any `ApplyScheduledSessionUpdates` event lacks the
                `frame_number` argument, or if `sched` scheduling records are
                missing from a trace that has matched render threads.
        """
        with PerfettoTraceProcessor(trace_path=fxt_path) as tp:
            start_row = tp.run_query(_START_EVENTS_QUERY)[0]
            if start_row["missing_frame_number_ts"] is not None:
                missing_ts = trace_time.TimePoint(
                    int(start_row["missing_frame_number_ts"])
                )
                raise ValueError(
                    f"Trace event '{_SCENIC_START_EVENT_NAME}' at {missing_ts} "
                    f"is missing the load-bearing '{_FRAME_NUMBER_ARG_NAME}' argument."
                )

            if int(start_row["start_count"]) == 0:
                _LOGGER.warning(
                    "No render or vsync events are present. Perhaps the trace "
                    "duration is too short to provide scenic render information"
                )
                return []

            frame_rows = tp.run_query(_RENDER_VSYNC_QUERY)
            if not frame_rows:
                _LOGGER.warning(
                    "No render or vsync events are present. Perhaps the trace "
                    "duration is too short to provide scenic render information"
                )
                return []

            windows_by_utid: collections.defaultdict[
                int, list[tuple[int, int, int]]
            ] = collections.defaultdict(list)
            for idx, row in enumerate(frame_rows):
                raw_utid = row["render_utid"]
                if raw_utid is None:
                    _LOGGER.warning(
                        "RenderFrame at start_ts=%s has no thread track "
                        "(render_utid is None); skipping RenderCpu sample.",
                        row["start_ts"],
                    )
                    continue
                windows_by_utid[int(raw_utid)].append(
                    (idx, int(row["start_ts"]), int(row["render_end_ts"]))
                )

            # Build `[start, end)` running segments directly in one pass over `sched` rows.
            # Perfetto populates `sched` intervals starting at the first observed
            # switch-in event for a thread, so running time before the first
            # captured switch-in is not included.
            utids = sorted(windows_by_utid)
            segments_by_utid: collections.defaultdict[
                int, list[tuple[int, int]]
            ] = collections.defaultdict(list)
            last_sched_by_utid: dict[int, tuple[int, int]] = {}
            if utids:
                _, row_iter = tp.query_rows(_build_sched_query(utids))
                for raw_utid, raw_ts, raw_dur in row_iter:
                    utid = int(raw_utid)
                    ts = int(raw_ts)
                    dur = int(raw_dur)
                    if dur > 0:
                        segments_by_utid[utid].append((ts, ts + dur))
                    last_sched_by_utid[utid] = (ts, dur)
                if not last_sched_by_utid and not tp.run_query(
                    _CHECK_SCHED_EXISTS_QUERY
                ):
                    raise ValueError(
                        "Scheduling records are missing from the trace."
                    )

        running_ns: list[int | None] = [None] * len(frame_rows)
        for utid, indexed_windows in windows_by_utid.items():
            segments = segments_by_utid.pop(utid, [])
            last_window_end = max(w_end for _, _, w_end in indexed_windows)
            if (last_sched := last_sched_by_utid.get(utid)) is not None:
                last_ts, last_dur = last_sched
                if last_dur == -1 and last_ts < last_window_end:
                    # Open-ended switch at trace end runs through last window.
                    segments.append((last_ts, last_window_end))

            windows = [
                (w_start, w_end) for _, w_start, w_end in indexed_windows
            ]
            overlaps = _compute_window_overlaps(segments, windows)
            for (orig_idx, _, _), overlap_ns in zip(
                indexed_windows, overlaps, strict=True
            ):
                running_ns[orig_idx] = overlap_ns

        cpu_render_times = [
            trace_time.TimeDelta(ns).to_milliseconds_f()
            for ns in running_ns
            if ns is not None
        ]
        metrics_list: list[tuple[str, list[float]]] = []
        if cpu_render_times:
            cpu_render_mean: float = statistics.mean(cpu_render_times)
            _LOGGER.info(f"Average CPU render time: {cpu_render_mean} ms")
            metrics_list.append(("RenderCpu", cpu_render_times))

        if self.include_render_total:
            total_render_times = [
                trace_time.TimeDelta(
                    int(row["vsync_ts"]) - int(row["start_ts"])
                ).to_milliseconds_f()
                for row in frame_rows
            ]
            total_render_mean: float = statistics.mean(total_render_times)
            _LOGGER.info(f"Average Total render time: {total_render_mean} ms")
            metrics_list.append(("RenderTotal", total_render_times))

        return self._format_results(metrics_list)

    def _format_results(
        self, metrics_list: Sequence[tuple[str, Sequence[float]]]
    ) -> list[metrics.TestCaseResult]:
        test_case_results: list[metrics.TestCaseResult] = []
        for name, values in metrics_list:
            if self.aggregates_only:
                test_case_results.extend(
                    trace_utils.standard_metrics_set(
                        values=list(values),
                        label_prefix=name,
                        unit=metrics.Unit.milliseconds,
                    )
                )
            else:
                test_case_results.append(
                    metrics.TestCaseResult(
                        name, metrics.Unit.milliseconds, list(values)
                    )
                )
        return test_case_results
