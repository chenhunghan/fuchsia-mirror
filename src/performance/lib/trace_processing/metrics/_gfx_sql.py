#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Shared Perfetto SQL and FPS helpers for graphics trace metrics."""

import itertools
from collections.abc import Sequence

from trace_processing import trace_time

EVENT_CATEGORY = "gfx"
DISPLAY_VSYNC_EVENT_NAME = "Flatland::DisplayCompositor::OnVsync"


def escape_sql_literal(value: str) -> str:
    """Escapes single quotes for safe interpolation into a SQL string literal."""
    return value.replace("'", "''")


def build_edges_table_sql(table_name: str) -> str:
    """Builds the SQL DDL materializing the `gfx` reachability edge table.

    Uses `0 AS edge_weight` so `graph_reachable_weight_bounded_dfs!` with bound
    `1` leaves the traversal unbounded by weight (only using the macro because
    it outputs `root_node_id`; bounding by weight would halt DFS in visit order
    rather than timestamp order).

    Complete (`dur > 0`) `OnVsync` slices are terminal (outgoing flows and
    children are excluded) so DFS never continues into subsequent frames. Instant
    (`dur = 0`) child -> enclosing parent edges let flows landing on an instant
    slice step up to the enclosing duration; downstream queries filter
    `target.ts >= source.ts` so backward parent steps never pick prior vsyncs.
    """
    return f"""\
INCLUDE PERFETTO MODULE graphs.search;

CREATE OR REPLACE PERFETTO TABLE {table_name} AS
-- Flow edges within '{EVENT_CATEGORY}', excluding outgoing flows from terminal
-- (dur > 0) OnVsync slices so DFS never continues into subsequent frames.
SELECT
  f.slice_out AS source_node_id,
  f.slice_in AS dest_node_id,
  0 AS edge_weight
FROM flow AS f
JOIN slice AS src ON src.id = f.slice_out
JOIN slice AS dst ON dst.id = f.slice_in
WHERE f.slice_out IS NOT NULL
  AND f.slice_in IS NOT NULL
  AND src.category = '{EVENT_CATEGORY}'
  AND dst.category = '{EVENT_CATEGORY}'
  AND NOT (
    src.name IS '{DISPLAY_VSYNC_EVENT_NAME}'
    AND src.dur > 0
  )
UNION ALL
-- Enclosing -> child edges within '{EVENT_CATEGORY}', excluding children of
-- terminal (dur > 0) OnVsync slices.
SELECT
  c.parent_id AS source_node_id,
  c.id AS dest_node_id,
  0 AS edge_weight
FROM slice AS c
JOIN slice AS p ON p.id = c.parent_id
WHERE c.parent_id IS NOT NULL
  AND c.category = '{EVENT_CATEGORY}'
  AND p.category = '{EVENT_CATEGORY}'
  AND NOT (
    p.name IS '{DISPLAY_VSYNC_EVENT_NAME}'
    AND p.dur > 0
  )
UNION ALL
-- Instant child (dur = 0) -> enclosing duration within '{EVENT_CATEGORY}'.
SELECT
  c.id AS source_node_id,
  c.parent_id AS dest_node_id,
  0 AS edge_weight
FROM slice AS c
JOIN slice AS p ON p.id = c.parent_id
WHERE c.dur = 0
  AND c.parent_id IS NOT NULL
  AND c.category = '{EVENT_CATEGORY}'
  AND p.category = '{EVENT_CATEGORY}';"""


def build_common_start_ctes_sql(edges_table: str, first_slice_name: str) -> str:
    """Builds CTE definitions computing `_common_start` for a flow graph.

    Skips early events caused by a trace start race across Scenic, Magma, and
    Display processes (see https://fxbug.dev/322849857) by anchoring to the
    latest first-event timestamp among processes reachable from the first
    complete `first_slice_name` slice.
    """
    escaped_name = escape_sql_literal(first_slice_name)
    return f"""\
_first_event AS (
  SELECT id
  FROM slice
  WHERE category = '{EVENT_CATEGORY}'
    AND name = '{escaped_name}'
    AND dur > 0
  ORDER BY ts ASC, id ASC
  LIMIT 1
),
_flow_slices AS (
  SELECT id AS slice_id FROM _first_event
  UNION
  SELECT r.node_id AS slice_id
  FROM graph_reachable_weight_bounded_dfs!(
    {edges_table},
    (SELECT id AS root_node_id, 1 AS root_target_weight FROM _first_event),
    1
  ) AS r
),
_flow_upids AS (
  SELECT DISTINCT COALESCE(th.upid, pt.upid) AS upid
  FROM _flow_slices AS fs
  JOIN slice AS s ON s.id = fs.slice_id
  LEFT JOIN thread_track AS tt ON s.track_id = tt.id
  LEFT JOIN thread AS th ON tt.utid = th.utid
  LEFT JOIN process_track AS pt ON s.track_id = pt.id
  WHERE COALESCE(th.upid, pt.upid) IS NOT NULL
),
_upid_tracks AS (
  SELECT tt.id AS track_id, th.upid AS upid
  FROM thread_track AS tt
  JOIN thread AS th ON tt.utid = th.utid
  WHERE th.upid IN (SELECT upid FROM _flow_upids)
  UNION ALL
  SELECT pt.id AS track_id, pt.upid AS upid
  FROM process_track AS pt
  WHERE pt.upid IN (SELECT upid FROM _flow_upids)
),
_common_start AS (
  SELECT MAX(min_ts) AS start_ts
  FROM (
    SELECT tr.upid, MIN(s.ts) AS min_ts
    FROM slice AS s
    JOIN _upid_tracks AS tr ON s.track_id = tr.track_id
    GROUP BY tr.upid
  )
)"""


def build_flow_vsync_query(
    edges_table: str,
    events_table: str,
    event_id_col: str,
    min_ts_col: str,
) -> str:
    """Builds the query mapping each event row to its earliest reachable OnVsync."""
    return f"""\
SELECT
  r.root_node_id AS {event_id_col},
  MIN(s.ts) AS vsync_ts
FROM graph_reachable_weight_bounded_dfs!(
  {edges_table},
  (SELECT id AS root_node_id, 1 AS root_target_weight FROM {events_table}),
  1
) AS r
JOIN {events_table} AS ev ON ev.id = r.root_node_id
JOIN slice AS s ON s.id = r.node_id
WHERE s.category = '{EVENT_CATEGORY}'
  AND s.name = '{DISPLAY_VSYNC_EVENT_NAME}'
  AND s.dur > 0
  AND s.ts >= ev.{min_ts_col}
GROUP BY r.root_node_id;
"""


# Include dur = 0 instant vsync slices in the fallback scan while still
# excluding dur = -1 unterminated slices (whereas `build_flow_vsync_query`
# filters to complete dur > 0 vsync slices).
FALLBACK_VSYNC_QUERY = f"""\
SELECT ts
FROM slice
WHERE category = '{EVENT_CATEGORY}'
  AND name = '{DISPLAY_VSYNC_EVENT_NAME}'
  AND dur >= 0
ORDER BY ts ASC;
"""


def compute_fps_values(vsync_timestamps: Sequence[int]) -> list[float]:
    """Computes per-frame FPS values from ordered vsync timestamps in nanoseconds."""
    fps_values: list[float] = []
    for prev_ts, next_ts in itertools.pairwise(vsync_timestamps):
        # Two renders may be squashed into one.
        if next_ts == prev_ts:
            continue
        fps_values.append(
            trace_time.TimeDelta.from_seconds(1)
            / trace_time.TimeDelta(next_ts - prev_ts)
        )
    return fps_values
