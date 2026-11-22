# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Thread-execution critical-path engine and standalone plugin for perf-analyze.

Computes the non-overlapping critical path across kernel scheduler wakeups
(`sched.thread_executing_span`), trace flow handoffs (`flow`), kernel thread
states (`thread_state`), and innermost thread slices (`slice`).

Both endpoints of a critical-path query must resolve to thread-bound points
(`(utid, ts)`), specified via:
  1. A `thread_track` slice ID (`--start-slice-id` / `--end-slice-id`),
  2. A `thread_track` slice name/glob (`--start-slice` / `--end-slice`), or
  3. An explicit thread (`--*-tid` or `--*-utid`) plus nanosecond timestamp (`--*-ts`).

Slices on `process_track`, async tracks, or virtual tracks have no `utid` and
cannot anchor a scheduler wakeup walk directly; `resolve_endpoint` detects them
and reports an explicit diagnostic error.
"""

from __future__ import annotations

import heapq
from collections import defaultdict
from dataclasses import dataclass
from typing import Any, Literal, Sequence

from plugins import (
    AnalyzePlugin,
    PluginArgumentError,
    PluginArgumentParser,
    SectionResult,
)
from tp_shell import PerfettoTraceProcessor

EdgeKind = Literal["start", "end"]

UNINSTRUMENTED_SLICE = "<uninstrumented>"

# Maximum causal chain steps when walking backward across wakeups.
_MAX_WAKEUP_WALK_STEPS = 10000
# Pre-start lookback margin (50 ms) so an executing span that began slightly
# before `start_ts` and is still active at `start_ts` stays in the search graph.
_SEARCH_LOOKBACK_NS = 50_000_000
# Maximum duration (100 ms) of a `flow` edge considered during fallback BFS so
# reused/long-lived async flow IDs do not jump across unrelated idle periods.
_MAX_FLOW_EDGE_SPAN_NS = 100_000_000


class EndpointResolutionError(Exception):
    """Raised when a start or end trace selector cannot resolve to `(utid, ts)`."""


@dataclass(frozen=True)
class SliceIdEndpoint:
    """Thread-bound slice endpoint selected by exact Perfetto `slice.id`."""

    slice_id: int
    edge: EdgeKind = "start"


@dataclass(frozen=True)
class SliceGlobEndpoint:
    """Thread-bound slice endpoint selected by `slice.name` GLOB and optional filters."""

    slice_glob: str
    process_glob: str | None = None
    thread_glob: str | None = None
    occurrence_index: int | None = None
    edge: EdgeKind = "start"


@dataclass(frozen=True)
class ThreadTimestampEndpoint:
    """Thread-bound timestamp endpoint selected by `(tid | utid)` and `ts` in ns."""

    ts: int
    tid: int | None = None
    utid: int | None = None
    process_glob: str | None = None
    thread_glob: str | None = None

    def __post_init__(self) -> None:
        if (self.tid is None) == (self.utid is None):
            raise ValueError(
                "ThreadTimestampEndpoint requires exactly one of tid or utid."
            )


EndpointSpec = SliceIdEndpoint | SliceGlobEndpoint | ThreadTimestampEndpoint


@dataclass(frozen=True)
class ResolvedEndpoint:
    """A validated thread-bound point `(utid, ts)` in the trace."""

    utid: int
    tid: int | None
    thread_name: str | None
    upid: int | None
    pid: int | None
    process_name: str | None
    ts: int
    slice_id: int | None = None
    slice_name: str | None = None
    edge: EdgeKind | None = None
    slice_ts: int | None = None

    def format_thread_label(self) -> str:
        proc = self.process_name or (
            f"pid:{self.pid}" if self.pid is not None else "<unknown>"
        )
        thr = self.thread_name or (
            f"tid:{self.tid}" if self.tid is not None else f"utid:{self.utid}"
        )
        return f"{proc}:{thr}"

    def describe(self) -> str:
        thread_desc = (
            f"{self.format_thread_label()} "
            f"(tid={self.tid}, utid={self.utid})"
        )
        if self.slice_id is not None:
            return (
                f"slice '{self.slice_name}' (id={self.slice_id}, edge={self.edge}) "
                f"on {thread_desc} @ {self.ts} ns"
            )
        return f"{thread_desc} @ {self.ts} ns"


@dataclass(frozen=True)
class SpineSegment:
    """A contiguous `[ts, ts + dur)` spine interval on `utid` within `group_id`."""

    group_id: int
    seg_index: int
    utid: int
    ts: int
    dur: int
    handoff_reason: str = ""


@dataclass(frozen=True)
class FlatCriticalPathSpan:
    """An atomic, non-overlapping critical-path interval with thread state and leaf slice."""

    group_id: int
    seg_index: int
    ts: int
    dur: int
    utid: int
    tid: int | None
    thread_name: str | None
    pid: int | None
    process_name: str | None
    state: str
    cpu: int | None
    slice_name: str
    category: str | None
    waker_utid: int | None
    waker_thread_name: str | None
    waker_process_name: str | None
    seg_handoff_reason: str
    slice_id: int | None = None


@dataclass
class GroupCriticalPathBreakdown:
    """Aggregated critical-path views for a single group (query window or launch)."""

    group_id: int
    start_ts: int
    total_dur_ns: int
    thread_summary: list[dict[str, Any]]
    top_slices: list[dict[str, Any]]
    timeline: list[dict[str, Any]]
    total_timeline_spans: int = 0


def _sql_quote(value: str) -> str:
    """Escapes single quotes for safe embedding in a PerfettoSQL string literal."""
    return "'" + value.replace("'", "''") + "'"


def _format_actor(
    proc_name: str | None, thr_name: str | None, utid: int | None
) -> str:
    proc = proc_name or "<kernel>"
    thr = thr_name or (f"utid:{utid}" if utid is not None else "<unknown>")
    return f"{proc}:{thr}"


def _slice_edge_ts(
    row: dict[str, Any], edge: EdgeKind, *, role: str = "endpoint"
) -> int:
    ts = int(row["ts"])
    if edge == "start":
        return ts
    raw_dur = row.get("dur")
    if raw_dur is None or int(raw_dur) < 0:
        raise EndpointResolutionError(
            f"{role.capitalize()} slice id={row.get('slice_id')} "
            f"('{row.get('slice_name')}') is incomplete (dur={raw_dur}) and "
            f"has no end timestamp; use --{role}-edge start or --{role}-ts."
        )
    return ts + int(raw_dur)


def _resolve_slice_endpoint(
    tp: PerfettoTraceProcessor,
    db_objects: set[str],
    spec: SliceIdEndpoint | SliceGlobEndpoint,
    *,
    role: str,
    min_ts: int | None = None,
) -> ResolvedEndpoint:
    """Resolves a slice ID or slice name GLOB endpoint via a single PerfettoSQL query."""
    required = {"slice", "thread_track", "thread"}
    if missing := required - db_objects:
        raise EndpointResolutionError(
            f"Required schema tables/views missing for {role} slice lookup: "
            f"{', '.join(sorted(missing))}"
        )

    has_process = "process" in db_objects
    if (
        isinstance(spec, SliceGlobEndpoint)
        and spec.process_glob is not None
        and not has_process
    ):
        raise EndpointResolutionError(
            f"Required schema tables/views missing for --{role}-process: process"
        )

    has_track = "track" in db_objects
    proc_join = "LEFT JOIN process p ON t.upid = p.upid" if has_process else ""
    proc_cols = (
        "t.upid AS upid, p.pid AS pid, p.name AS process_name,"
        if has_process
        else "NULL AS upid, NULL AS pid, NULL AS process_name,"
    )
    track_join = "LEFT JOIN track tr ON s.track_id = tr.id" if has_track else ""
    track_cols = (
        "tr.name AS track_name, tr.type AS track_type"
        if has_track
        else "NULL AS track_name, NULL AS track_type"
    )

    if isinstance(spec, SliceIdEndpoint):
        where_sql = f"s.id = {int(spec.slice_id)}"
        limit_sql = "LIMIT 1"
    else:
        predicates = [f"s.name GLOB {_sql_quote(spec.slice_glob)}"]
        if spec.thread_glob is not None:
            predicates.append(
                f"(tt.utid IS NULL OR (t.name IS NOT NULL AND t.name GLOB {_sql_quote(spec.thread_glob)}))"
            )
        if spec.process_glob is not None:
            predicates.append(
                f"(tt.utid IS NULL OR (p.name IS NOT NULL AND p.name GLOB {_sql_quote(spec.process_glob)}))"
            )
        where_sql = " AND ".join(predicates)
        limit_sql = ""

    rows = tp.run_query(
        f"""
        SELECT
          s.id AS slice_id,
          s.name AS slice_name,
          s.ts AS ts,
          s.dur AS dur,
          tt.utid AS utid,
          t.tid AS tid,
          t.name AS thread_name,
          {proc_cols}
          {track_cols}
        FROM slice s
        LEFT JOIN thread_track tt ON s.track_id = tt.id
        LEFT JOIN thread t ON tt.utid = t.utid
        {proc_join}
        {track_join}
        WHERE {where_sql}
        ORDER BY (tt.utid IS NULL) ASC, s.ts ASC, s.id ASC
        {limit_sql};
        """
    )

    thread_rows = [r for r in rows if r["utid"] is not None]
    non_thread_rows = [r for r in rows if r["utid"] is None]

    if not thread_rows:
        if (
            non_thread_rows
            and not (
                isinstance(spec, SliceGlobEndpoint)
                and (
                    spec.thread_glob is not None
                    or spec.process_glob is not None
                )
            )
        ) or (isinstance(spec, SliceIdEndpoint) and non_thread_rows):
            nt = non_thread_rows[0]
            track_desc = (
                f"track '{nt['track_name']}' (type='{nt['track_type']}')"
                if nt.get("track_name") or nt.get("track_type")
                else "a non-thread track"
            )
            selector_desc = (
                f"id={spec.slice_id} ('{nt['slice_name']}')"
                if isinstance(spec, SliceIdEndpoint)
                else f"pattern '{spec.slice_glob}' matched slice id={nt['slice_id']} ('{nt['slice_name']}')"
            )
            raise EndpointResolutionError(
                f"{role.capitalize()} slice {selector_desc} on {track_desc} "
                "has no utid. Critical-path endpoints must resolve to a "
                "thread-bound slice (on thread_track) or explicit tid/utid + ts."
            )
        if isinstance(spec, SliceIdEndpoint):
            raise EndpointResolutionError(
                f"{role.capitalize()} slice id={spec.slice_id} was not found in the trace."
            )
        raise EndpointResolutionError(
            f"No thread-bound slice matched {role} pattern '{spec.slice_glob}'."
        )

    if isinstance(spec, SliceIdEndpoint):
        chosen = thread_rows[0]
    elif spec.occurrence_index is not None:
        idx = spec.occurrence_index
        if idx < -len(thread_rows) or idx >= len(thread_rows):
            raise EndpointResolutionError(
                f"{role.capitalize()} index {idx} out of range for pattern "
                f"'{spec.slice_glob}' (matched {len(thread_rows)} slice(s))."
            )
        chosen = thread_rows[idx]
    else:
        candidates = (
            [
                r
                for r in thread_rows
                if r.get("dur") is not None and int(r["dur"]) >= 0
            ]
            if spec.edge == "end"
            else thread_rows
        )
        if not candidates:
            raise EndpointResolutionError(
                f"{role.capitalize()} slice pattern '{spec.slice_glob}' matched "
                f"{len(thread_rows)} slice(s), all incomplete (dur=-1) with no "
                f"end timestamp; use --{role}-edge start or --{role}-ts."
            )
        if min_ts is not None:
            after_start = [
                r
                for r in candidates
                if _slice_edge_ts(r, spec.edge, role=role) > min_ts
            ]
            if not after_start:
                raise EndpointResolutionError(
                    f"{role.capitalize()} slice pattern '{spec.slice_glob}' matched "
                    f"{len(candidates)} thread slice(s), but none occurred after start_ts={min_ts} ns."
                )
            chosen = min(
                after_start,
                key=lambda r: (
                    _slice_edge_ts(r, spec.edge, role=role),
                    int(r["slice_id"]),
                ),
            )
        else:
            chosen = candidates[0]

    resolved_ts = _slice_edge_ts(chosen, spec.edge, role=role)
    return ResolvedEndpoint(
        utid=int(chosen["utid"]),
        tid=int(chosen["tid"]) if chosen["tid"] is not None else None,
        thread_name=chosen["thread_name"],
        upid=int(chosen["upid"]) if chosen["upid"] is not None else None,
        pid=int(chosen["pid"]) if chosen["pid"] is not None else None,
        process_name=chosen["process_name"],
        ts=resolved_ts,
        slice_id=int(chosen["slice_id"]),
        slice_name=chosen["slice_name"],
        edge=spec.edge,
        slice_ts=int(chosen["ts"]),
    )


def resolve_endpoint(
    tp: PerfettoTraceProcessor,
    db_objects: set[str],
    spec: EndpointSpec,
    *,
    role: str,
    min_ts: int | None = None,
) -> ResolvedEndpoint:
    """Resolves `spec` to a validated thread-bound `ResolvedEndpoint`."""
    if isinstance(spec, (SliceIdEndpoint, SliceGlobEndpoint)):
        return _resolve_slice_endpoint(
            tp, db_objects, spec, role=role, min_ts=min_ts
        )

    # Explicit `(tid | utid) + ts` selector (`ThreadTimestampEndpoint`).
    if "thread" not in db_objects:
        raise EndpointResolutionError(
            f"Required schema tables/views missing for {role} thread lookup: thread"
        )
    has_process = "process" in db_objects
    if spec.process_glob is not None and not has_process:
        raise EndpointResolutionError(
            f"Required schema tables/views missing for --{role}-process: process"
        )
    proc_join = "LEFT JOIN process p ON t.upid = p.upid" if has_process else ""
    proc_cols = (
        "t.upid AS upid, p.pid AS pid, p.name AS process_name"
        if has_process
        else "NULL AS upid, NULL AS pid, NULL AS process_name"
    )
    predicates = []
    if spec.utid is not None:
        predicates.append(f"t.utid = {int(spec.utid)}")
    else:
        assert spec.tid is not None
        predicates.append(f"t.tid = {int(spec.tid)}")
    if spec.thread_glob is not None:
        predicates.append(
            f"(t.name IS NOT NULL AND t.name GLOB {_sql_quote(spec.thread_glob)})"
        )
    if spec.process_glob is not None:
        predicates.append(
            f"(p.name IS NOT NULL AND p.name GLOB {_sql_quote(spec.process_glob)})"
        )
    where_sql = " AND ".join(predicates)
    rows = tp.run_query(
        f"""
        SELECT
          t.utid AS utid,
          t.tid AS tid,
          t.name AS thread_name,
          t.start_ts AS start_ts,
          t.end_ts AS end_ts,
          {proc_cols}
        FROM thread t
        {proc_join}
        WHERE {where_sql}
        ORDER BY t.utid ASC;
        """
    )
    if not rows:
        ident = (
            f"utid={spec.utid}" if spec.utid is not None else f"tid={spec.tid}"
        )
        raise EndpointResolutionError(
            f"No thread matched {role} selector ({ident})."
        )

    chosen_row = rows[0]
    if len(rows) > 1:
        # Prefer the thread lifetime covering `spec.ts` when TID reuse occurs.
        covering = [
            r
            for r in rows
            if (r["start_ts"] is None or int(r["start_ts"]) <= spec.ts)
            and (r["end_ts"] is None or int(r["end_ts"]) >= spec.ts)
        ]
        if len(covering) == 1:
            chosen_row = covering[0]
        else:
            utids_str = ", ".join(str(r["utid"]) for r in rows[:10])
            raise EndpointResolutionError(
                f"Ambiguous {role} selector tid={spec.tid} matched {len(rows)} "
                f"threads (utids: {utids_str}). Disambiguate with --{role}-utid, "
                f"--{role}-process, or --{role}-thread."
            )

    return ResolvedEndpoint(
        utid=int(chosen_row["utid"]),
        tid=int(chosen_row["tid"]) if chosen_row["tid"] is not None else None,
        thread_name=chosen_row["thread_name"],
        upid=int(chosen_row["upid"])
        if chosen_row["upid"] is not None
        else None,
        pid=int(chosen_row["pid"]) if chosen_row["pid"] is not None else None,
        process_name=chosen_row["process_name"],
        ts=int(spec.ts),
    )


def _build_pa_nodes_sql(
    group_windows: Sequence[tuple[int, int]],
    max_window_ts: int,
) -> str:
    """Builds the SQL DDL materializing `_pa_nodes` from `sched.thread_executing_span`.

    Enriches `_wakeup_graph` with:
      1. Clamped `dur = -1` open-ended `thread_state` rows at trace EOF so threads
         still `Running` when tracing stops retain full duration, flow overlap,
         and `waker_id` attribution.
      2. Bootstrap initial spans for threads already active (`Running`/`R`/`R+`/`W`)
         at trace start before their first sleep (`dur > 0`), without overwriting
         non-null `wg.idle_dur`.
      3. Per-group bounded `idle_dur` on newly created threads (`prev_id IS NULL`
         and `idle_dur IS NULL` with `waker_id IS NOT NULL`) so root-level walks
         in `WalkOneRoot` chain backward into their creator/fork waker down to the
         enclosing group's start timestamp.
      4. Pre-aggregated per-`utid` `max_active_end_ts` and `last_id` to fix
         truncated final `_wakeup_graph` durations without correlated subqueries.
    """
    effective_windows = (
        group_windows if group_windows else [(0, max(0, int(max_window_ts)))]
    )
    windows_values = ", ".join(
        f"({int(lo)}, {int(hi)})" for lo, hi in effective_windows
    )
    return f"""
    INCLUDE PERFETTO MODULE sched.thread_executing_span;

    CREATE OR REPLACE PERFETTO TABLE _pa_nodes AS
    WITH
      eof_bound AS MATERIALIZED (
        SELECT MAX(
          {int(max_window_ts)},
          COALESCE((SELECT MAX(ts + IIF(dur > 0, dur, 0)) FROM thread_state), 0)
        ) AS eof_ts
      ),
      group_windows(min_ts, max_ts) AS MATERIALIZED (
        VALUES {windows_values}
      ),
      ts_stats_per_utid AS MATERIALIZED (
        SELECT
          ts_row.utid,
          MIN(ts_row.id) AS first_id,
          MIN(
            CASE WHEN ts_row.state IN ('S', 'D', 'I') THEN ts_row.ts END
          ) AS first_sleep_ts,
          MAX(
            CASE
              WHEN ts_row.state IN ('Running', 'R', 'R+', 'W') THEN
                ts_row.ts + CASE
                  WHEN ts_row.dur = -1 THEN MAX(0, eb.eof_ts - ts_row.ts)
                  ELSE ts_row.dur
                END
            END
          ) AS max_active_end_ts
        FROM thread_state ts_row, eof_bound eb
        WHERE ts_row.dur >= -1
        GROUP BY ts_row.utid
      ),
      bootstrap_utids AS MATERIALIZED (
        SELECT *
        FROM (
          SELECT
            ts_row.id,
            ts_row.utid,
            ts_row.ts,
            COALESCE(
              IIF(st.first_sleep_ts >= ts_row.ts, st.first_sleep_ts, NULL),
              st.max_active_end_ts,
              ts_row.ts
            ) - ts_row.ts AS dur
          FROM ts_stats_per_utid st
          JOIN thread_state ts_row ON ts_row.id = st.first_id
          LEFT JOIN _wakeup_graph wg ON wg.id = ts_row.id
          WHERE wg.id IS NULL
            AND ts_row.state IN ('Running', 'R', 'R+', 'W')
        )
        WHERE dur > 0
      ),
      last_wg_per_utid AS MATERIALIZED (
        SELECT utid, MAX(id) AS last_id
        FROM _wakeup_graph
        GROUP BY utid
      ),
      fixed_last_wg AS MATERIALIZED (
        SELECT
          wg.id AS wakeup_id,
          MAX(wg.dur, COALESCE(st.max_active_end_ts - wg.ts, wg.dur)) AS fixed_dur
        FROM last_wg_per_utid l
        JOIN _wakeup_graph wg ON wg.id = l.last_id
        LEFT JOIN ts_stats_per_utid st ON st.utid = wg.utid
      ),
      missing_wakers AS MATERIALIZED (
        SELECT
          wu.id AS wakeup_id,
          COALESCE(b_waker.id, l_waker.last_id) AS waker_node_id
        FROM _wakeup wu
        JOIN thread_state w_ts ON w_ts.id = wu.waker_id
        LEFT JOIN bootstrap_utids b_waker
          ON b_waker.utid = wu.waker_utid
          AND w_ts.ts >= b_waker.ts
          AND w_ts.ts < b_waker.ts + b_waker.dur
        LEFT JOIN last_wg_per_utid l_waker
          ON l_waker.utid = wu.waker_utid
          AND w_ts.dur = -1
        LEFT JOIN _wakeup_graph wg_last ON wg_last.id = l_waker.last_id
        WHERE wu.is_irq = 0
          AND (
            b_waker.id IS NOT NULL
            OR (w_ts.dur = -1 AND wg_last.ts <= w_ts.ts)
          )
      )
    SELECT
      wg.id,
      wg.utid,
      wg.ts,
      COALESCE(fl.fixed_dur, wg.dur) AS dur,
      CASE
        WHEN wg.prev_id IS NULL
          AND wg.idle_dur IS NULL
          AND COALESCE(wg.waker_id, mw.waker_node_id) IS NOT NULL
          THEN (
            SELECT wg.ts - MIN(gw.min_ts)
            FROM group_windows gw
            WHERE wg.ts > gw.min_ts AND wg.ts <= gw.max_ts
          )
        ELSE wg.idle_dur
      END AS idle_dur,
      COALESCE(wg.waker_id, mw.waker_node_id) AS waker_id,
      COALESCE(wg.prev_id, b.id) AS prev_id
    FROM _wakeup_graph wg
    LEFT JOIN fixed_last_wg fl ON fl.wakeup_id = wg.id
    LEFT JOIN bootstrap_utids b
      ON b.utid = wg.utid AND wg.prev_id IS NULL AND b.ts + b.dur <= wg.ts
    LEFT JOIN missing_wakers mw
      ON mw.wakeup_id = wg.id AND wg.waker_id IS NULL
    UNION ALL
    SELECT
      b.id,
      b.utid,
      b.ts,
      b.dur,
      NULL AS idle_dur,
      NULL AS waker_id,
      NULL AS prev_id
    FROM bootstrap_utids b
    -- Keep `id` sorted on the Perfetto table to match `_wakeup_graph`
    -- (sched/thread_executing_span.sql;l=263). Without this sort, `bootstrap_utids`
    -- appends smaller `first_id` rows at the tail and forces linear `id` scans.
    ORDER BY id;
    """


def merge_spine_segments(
    segments: Sequence[SpineSegment],
) -> list[SpineSegment]:
    """Merges adjacent contiguous `SpineSegment`s sharing `(group_id, utid)` and re-indexes `seg_index`."""
    merged: list[SpineSegment] = []
    group_counts: dict[int, int] = defaultdict(int)
    for seg in segments:
        if (
            merged
            and merged[-1].group_id == seg.group_id
            and merged[-1].utid == seg.utid
            and merged[-1].ts + merged[-1].dur == seg.ts
        ):
            prev = merged[-1]
            merged[-1] = SpineSegment(
                group_id=prev.group_id,
                seg_index=prev.seg_index,
                utid=prev.utid,
                ts=prev.ts,
                dur=prev.dur + seg.dur,
                handoff_reason=prev.handoff_reason,
            )
        else:
            idx = group_counts[seg.group_id]
            group_counts[seg.group_id] = idx + 1
            merged.append(
                SpineSegment(
                    group_id=seg.group_id,
                    seg_index=idx,
                    utid=seg.utid,
                    ts=seg.ts,
                    dur=seg.dur,
                    handoff_reason=seg.handoff_reason,
                )
            )
    return merged


def _chain_rows_to_spine_segments(
    chain_rows: Sequence[dict[str, Any]],
    start_ts: int,
    end_ts: int,
) -> list[SpineSegment]:
    """Converts a forward-ordered causal node chain `[N_0, ..., N_k]` into contiguous `SpineSegment`s."""
    if not chain_rows or end_ts <= start_ts:
        return []

    raw_segments: list[SpineSegment] = []
    cursor_ts = start_ts
    cur_utid = int(chain_rows[0]["utid"])
    cur_reason = "start"

    for idx in range(len(chain_rows) - 1):
        nxt = chain_rows[idx + 1]
        nxt_utid = int(nxt["utid"])
        if nxt_utid == cur_utid:
            continue

        handoff_raw = int(nxt["handoff_ts"])
        handoff_ts = max(cursor_ts, min(end_ts, handoff_raw))
        edge_reason = str(nxt.get("edge_kind") or "wakeup")
        if handoff_ts > cursor_ts:
            raw_segments.append(
                SpineSegment(
                    group_id=0,
                    seg_index=len(raw_segments),
                    utid=cur_utid,
                    ts=cursor_ts,
                    dur=handoff_ts - cursor_ts,
                    handoff_reason=cur_reason,
                )
            )
            cursor_ts = handoff_ts
        cur_utid = nxt_utid
        cur_reason = edge_reason

    if end_ts > cursor_ts:
        raw_segments.append(
            SpineSegment(
                group_id=0,
                seg_index=len(raw_segments),
                utid=cur_utid,
                ts=cursor_ts,
                dur=end_ts - cursor_ts,
                handoff_reason=cur_reason,
            )
        )

    return merge_spine_segments(raw_segments)


def _resolve_endpoint_nodes(
    nodes_by_utid: dict[int, list[dict[str, Any]]],
    start_ep: ResolvedEndpoint,
    end_ep: ResolvedEndpoint,
) -> tuple[dict[int, int], list[tuple[int, int, int, int, int]]]:
    """Resolves `start_ranks` and `end_seeds` from `_pa_nodes` rows on the endpoint threads."""
    s_nodes = nodes_by_utid.get(start_ep.utid, [])
    start_ranks: dict[int, int] = {}
    strictly_covers = False
    s_cover = [
        n for n in s_nodes if n["ts"] <= start_ep.ts <= n["ts"] + n["dur"]
    ]
    if s_cover:
        best_s = max(
            s_cover,
            key=lambda n: (n["ts"] + n["dur"] > start_ep.ts, n["ts"]),
        )
        start_ranks[int(best_s["id"])] = 1
        strictly_covers = (best_s["ts"] + best_s["dur"]) > start_ep.ts
    if not strictly_covers:
        s_next = [n for n in s_nodes if start_ep.ts < n["ts"] <= end_ep.ts]
        if s_next:
            best_nxt = min(s_next, key=lambda n: n["ts"])
            start_ranks.setdefault(int(best_nxt["id"]), 2)
    if not start_ranks:
        s_prev = [n for n in s_nodes if n["ts"] <= start_ep.ts]
        if s_prev:
            best_prev = max(s_prev, key=lambda n: n["ts"])
            start_ranks[int(best_prev["id"])] = 3

    e_nodes = nodes_by_utid.get(end_ep.utid, [])
    end_seeds_by_id: dict[int, tuple[int, int, int, int, int]] = {}
    e_cover = [n for n in e_nodes if n["ts"] <= end_ep.ts <= n["ts"] + n["dur"]]
    if e_cover:
        best_e = max(e_cover, key=lambda n: n["ts"])
        nid = int(best_e["id"])
        end_seeds_by_id[nid] = (
            1,
            nid,
            end_ep.ts,
            int(best_e["utid"]),
            int(best_e["ts"]),
        )
    else:
        e_sleep_wake = [
            n
            for n in e_nodes
            if n["ts"] > end_ep.ts and (n["ts"] - n["idle_dur"]) <= end_ep.ts
        ]
        if e_sleep_wake:
            sw = min(e_sleep_wake, key=lambda n: n["ts"])
            nid = int(sw["id"])
            end_seeds_by_id[nid] = (
                2,
                nid,
                int(sw["ts"]),
                int(sw["utid"]),
                int(sw["ts"]),
            )
        e_prev = [n for n in e_nodes if n["ts"] <= end_ep.ts]
        if e_prev:
            ep = max(e_prev, key=lambda n: n["ts"])
            nid = int(ep["id"])
            end_seeds_by_id.setdefault(
                nid,
                (3, nid, end_ep.ts, int(ep["utid"]), int(ep["ts"])),
            )

    end_seeds = sorted(end_seeds_by_id.values(), key=lambda s: s[0])
    return start_ranks, end_seeds


@dataclass(frozen=True)
class _CausalEdge:
    """Backward causal edge `source_node_id -> dest_node_id` for spine search."""

    source_node_id: int
    dest_node_id: int
    dest_utid: int
    dest_ts: int
    req_ts: int
    dest_latest_ts: int
    handoff_ts: int
    priority: int
    fallback_cost: int
    edge_kind: str


def _search_causal_chain(
    start_ranks: dict[int, int],
    end_seeds: Sequence[tuple[int, int, int, int, int]],
    edges: Sequence[_CausalEdge],
    start_ts: int,
    min_causal_ts: int,
    *,
    prefer_pure_wakeup: bool = False,
) -> list[dict[str, Any]]:
    """Runs time-monotonic priority BFS backward from `end_seeds` to `start_ranks`.

    Enforces two invariants across backward hops `u -> v`:
      1. Causal monotonicity: an outgoing backward edge from `u` with arrival/event
         timestamp `req_ts` is valid only when `req_ts <= cur_ts` (so a flow
         arriving at `u` at `t2` cannot explain a wakeup `u` already emitted at
         `t1 < t2`).
      2. Priority tie-breaking:
         - When `prefer_pure_wakeup=True` (Tier 1), minimizes `fallback_steps`
           first (`0` along `COALESCE(waker_id, prev_id)`, `+1` only when
           falling back to `prev_id` past a dead-end `waker_id`), exploring the
           deterministic wakeup chain before any `prev_id` fallback.
         - When `prefer_pure_wakeup=False` (Tier 2), minimizes hop count first,
           then cumulative edge priority (`wakeup = 1 < flow = 2 < same_thread = 3`).
    """
    if not start_ranks or not end_seeds:
        return []

    adj: dict[int, list[_CausalEdge]] = defaultdict(list)
    for edge in edges:
        adj[edge.source_node_id].append(edge)
    for out_edges in adj.values():
        out_edges.sort(
            key=lambda e: (
                e.fallback_cost if prefer_pure_wakeup else 0,
                e.priority,
                e.handoff_ts,
            )
        )

    # State table: `(node_id, utid, node_ts, parent_state_idx, in_handoff_ts, in_edge_kind)`
    states: list[tuple[int, int, int, int | None, int, str]] = []
    # Heap item: `(end_rank, fallback_sum, hops, prio_sum, start_rank_hint, -cur_ts, state_idx)`
    heap: list[tuple[int, int, int, int, int, int, int]] = []
    best_latest_ts: dict[tuple[int, int], int] = {}

    for end_rank, node_id, seed_ts, utid, node_ts in sorted(
        end_seeds, key=lambda s: s[0]
    ):
        state_idx = len(states)
        states.append((node_id, utid, node_ts, None, seed_ts, "end"))
        s_rank = start_ranks.get(node_id, 999)
        heapq.heappush(heap, (end_rank, 0, 0, 0, s_rank, -seed_ts, state_idx))

    while heap:
        (
            end_rank,
            fallback_sum,
            hops,
            prio_sum,
            _,
            neg_cur_ts,
            state_idx,
        ) = heapq.heappop(heap)
        cur_ts = -neg_cur_ts
        node_id, _, _, _, _, _ = states[state_idx]

        if hops > 0 and node_id in start_ranks and cur_ts >= min_causal_ts:
            # Reconstruct forward chain `[start_node, ..., end_node]`.
            chain_rows: list[dict[str, Any]] = []
            cur_idx: int | None = state_idx
            step = 0
            prev_handoff = start_ts
            prev_kind = "start"
            while cur_idx is not None:
                s_nid, s_utid, s_ts, nxt_idx, edge_handoff, edge_kind = states[
                    cur_idx
                ]
                chain_rows.append(
                    {
                        "step": step,
                        "node_id": s_nid,
                        "utid": s_utid,
                        "ts": s_ts,
                        "handoff_ts": prev_handoff,
                        "edge_kind": prev_kind,
                    }
                )
                prev_handoff = edge_handoff
                prev_kind = edge_kind
                cur_idx = nxt_idx
                step += 1
            return chain_rows

        if hops >= _MAX_WAKEUP_WALK_STEPS:
            continue

        prev_best = best_latest_ts.get((end_rank, node_id), -1)
        if cur_ts <= prev_best:
            continue
        best_latest_ts[(end_rank, node_id)] = cur_ts

        for edge in adj.get(node_id, ()):
            if edge.req_ts > cur_ts:
                continue
            if edge.dest_latest_ts <= best_latest_ts.get(
                (end_rank, edge.dest_node_id), -1
            ):
                continue
            nxt_state_idx = len(states)
            states.append(
                (
                    edge.dest_node_id,
                    edge.dest_utid,
                    edge.dest_ts,
                    state_idx,
                    edge.handoff_ts,
                    edge.edge_kind,
                )
            )
            fb_delta = edge.fallback_cost if prefer_pure_wakeup else 0
            s_rank = start_ranks.get(edge.dest_node_id, 999)
            heapq.heappush(
                heap,
                (
                    end_rank,
                    fallback_sum + fb_delta,
                    hops + 1,
                    prio_sum + edge.priority,
                    s_rank,
                    -edge.dest_latest_ts,
                    nxt_state_idx,
                ),
            )

    return []


def find_cross_thread_spine(
    tp: PerfettoTraceProcessor,
    db_objects: set[str],
    start_ep: ResolvedEndpoint,
    end_ep: ResolvedEndpoint,
) -> tuple[list[SpineSegment], str]:
    """Finds the causal thread-handoff spine connecting `start_ep` to `end_ep`.

    When `start_ep.utid == end_ep.utid`, returns a single segment on that thread
    (intra-thread sleeps and cross-thread blockers are expanded automatically by
    `compute_critical_path`).

    When `start_ep.utid != end_ep.utid`:
      - Tier 1 searches the kernel wakeup + same-thread graph backward from
        `end_ep` to `start_ep`, preferring `COALESCE(waker_id, prev_id)` before
        falling back to `prev_id` past dead-end IRQ/timer wakers.
      - Tier 2 lazily queries bounded `flow` edges (`<= 100 ms`) when Tier 1 is
        unreachable and runs time-monotonic priority BFS to bridge handoffs across
        already-awake event-loop threads without fabricating reverse-time hops.
    """
    if start_ep.utid == end_ep.utid:
        return (
            [
                SpineSegment(
                    group_id=0,
                    seg_index=0,
                    utid=start_ep.utid,
                    ts=start_ep.ts,
                    dur=end_ep.ts - start_ep.ts,
                    handoff_reason="same_thread",
                )
            ],
            "single-thread spine",
        )

    min_causal_ts = (
        start_ep.slice_ts if start_ep.slice_ts is not None else start_ep.ts
    )
    lookback_ts = max(0, min_causal_ts - _SEARCH_LOOKBACK_NS)
    pa_nodes_ddl = _build_pa_nodes_sql([(start_ep.ts, end_ep.ts)], end_ep.ts)

    node_rows = tp.run_query(
        f"""
        {pa_nodes_ddl}

        WITH
          sleep_wake_bound AS (
            SELECT COALESCE(MIN(ts), {end_ep.ts}) AS max_ts
            FROM _pa_nodes
            WHERE utid = {end_ep.utid}
              AND ts > {end_ep.ts}
              AND (ts - COALESCE(idle_dur, 0)) <= {end_ep.ts}
              AND NOT EXISTS (
                SELECT 1
                FROM _pa_nodes
                WHERE utid = {end_ep.utid}
                  AND ts <= {end_ep.ts}
                  AND ts + COALESCE(dur, 0) >= {end_ep.ts}
              )
          )
        SELECT
          n.id,
          n.utid,
          n.ts,
          COALESCE(n.dur, 0) AS dur,
          COALESCE(n.idle_dur, 0) AS idle_dur,
          n.waker_id,
          n.prev_id
        FROM _pa_nodes n, sleep_wake_bound b
        WHERE n.ts <= b.max_ts
          AND (
            (n.ts + COALESCE(n.dur, 0)) >= {lookback_ts}
            OR n.utid IN ({start_ep.utid}, {end_ep.utid})
          )
        ORDER BY n.ts ASC, n.id ASC;
        """
    )

    nodes_by_id: dict[int, dict[str, Any]] = {}
    nodes_by_utid: dict[int, list[dict[str, Any]]] = defaultdict(list)
    for raw_node in node_rows:
        node_id = int(raw_node["id"])
        node_utid = int(raw_node["utid"])
        node: dict[str, Any] = {
            "id": node_id,
            "utid": node_utid,
            "ts": int(raw_node["ts"]),
            "dur": int(raw_node["dur"]),
            "idle_dur": int(raw_node["idle_dur"]),
            "waker_id": (
                int(raw_node["waker_id"])
                if raw_node["waker_id"] is not None
                else None
            ),
            "prev_id": (
                int(raw_node["prev_id"])
                if raw_node["prev_id"] is not None
                else None
            ),
        }
        nodes_by_id[node_id] = node
        nodes_by_utid[node_utid].append(node)

    start_ranks, end_seeds = _resolve_endpoint_nodes(
        nodes_by_utid, start_ep, end_ep
    )
    if not start_ranks or not end_seeds:
        return [], "unreachable"

    max_search_ts = max(end_ep.ts, *(s[2] for s in end_seeds))
    dedup_edges: dict[tuple[int, int, int], _CausalEdge] = {}

    def _put_edge(edge: _CausalEdge) -> None:
        key = (edge.source_node_id, edge.dest_node_id, edge.req_ts)
        cur = dedup_edges.get(key)
        if cur is None or (edge.priority, edge.handoff_ts) < (
            cur.priority,
            cur.handoff_ts,
        ):
            dedup_edges[key] = edge

    for n in nodes_by_id.values():
        if n["ts"] > max_search_ts or (n["ts"] + n["dur"]) < lookback_ts:
            continue
        nid, nts = n["id"], n["ts"]
        waker_id = n["waker_id"]
        for dst_id, prio, fb_cost, kind in (
            (waker_id, 1, 0, "wakeup"),
            (n["prev_id"], 3, 1 if waker_id is not None else 0, "same_thread"),
        ):
            if dst_id is None:
                continue
            dn = nodes_by_id.get(dst_id)
            if dn is not None and (
                (dn["ts"] + dn["dur"]) >= lookback_ts or dst_id in start_ranks
            ):
                _put_edge(
                    _CausalEdge(
                        nid,
                        dst_id,
                        dn["utid"],
                        dn["ts"],
                        nts,
                        max(dn["ts"], min(nts, dn["ts"] + dn["dur"])),
                        nts,
                        prio,
                        fb_cost,
                        kind,
                    )
                )

    tier1_rows = _search_causal_chain(
        start_ranks=start_ranks,
        end_seeds=end_seeds,
        edges=list(dedup_edges.values()),
        start_ts=start_ep.ts,
        min_causal_ts=min_causal_ts,
        prefer_pure_wakeup=True,
    )
    if tier1_rows:
        segs = _chain_rows_to_spine_segments(tier1_rows, start_ep.ts, end_ep.ts)
        return (
            segs,
            f"cross-thread wakeup spine ({max(0, len(segs) - 1)} thread handoff(s))",
        )

    if not {"flow", "slice", "thread_track"}.issubset(db_objects):
        return [], "unreachable"

    min_flow_out_ts = max(0, lookback_ts - _MAX_FLOW_EDGE_SPAN_NS)
    flow_rows = tp.run_query(
        f"""
        WITH
          win_nodes AS MATERIALIZED (
            SELECT id, utid, ts, COALESCE(dur, 0) AS dur
            FROM _pa_nodes
            WHERE ts <= {max_search_ts}
              AND (ts + COALESCE(dur, 0)) >= {lookback_ts}
          ),
          flow_raw AS (
            SELECT
              s_in.ts AS in_ts,
              COALESCE(s_in.dur, 0) AS in_dur,
              tt_in.utid AS in_utid,
              s_out.ts AS out_ts,
              s_out.category AS out_cat,
              tt_out.utid AS out_utid,
              MIN(
                s_in.ts,
                MAX(
                  s_out.ts,
                  IIF(s_out.dur < 0, {max_search_ts}, s_out.ts + s_out.dur)
                )
              ) AS emit_ts
            FROM flow f
            JOIN slice s_out ON f.slice_out = s_out.id
            JOIN slice s_in ON f.slice_in = s_in.id
            JOIN thread_track tt_out ON s_out.track_id = tt_out.id
            JOIN thread_track tt_in ON s_in.track_id = tt_in.id
            WHERE s_in.ts >= {lookback_ts}
              AND s_in.ts <= {max_search_ts}
              AND s_out.ts >= {min_flow_out_ts}
              AND s_out.ts <= s_in.ts
              AND (s_in.ts - s_out.ts) <= {_MAX_FLOW_EDGE_SPAN_NS}
          )
        SELECT
          n_in.id AS source_node_id,
          n_out.id AS dest_node_id,
          n_out.utid AS dest_utid,
          n_out.ts AS dest_ts,
          fr.in_ts AS req_ts,
          MAX(n_out.ts, MIN(fr.emit_ts, n_out.ts + n_out.dur)) AS dest_latest_ts,
          MAX(n_in.ts, fr.emit_ts) AS handoff_ts,
          'flow' || CASE
            WHEN MIN(fr.out_cat) IS NOT NULL AND MIN(fr.out_cat) != ''
              THEN ' (' || MIN(fr.out_cat) || ')'
            ELSE ''
          END AS edge_kind
        FROM flow_raw fr
        JOIN win_nodes n_out
          ON n_out.utid = fr.out_utid
          AND IIF(
            fr.emit_ts > fr.out_ts,
            n_out.ts < fr.emit_ts AND n_out.ts + n_out.dur >= fr.emit_ts,
            n_out.ts <= fr.emit_ts
              AND (
                n_out.ts + n_out.dur > fr.emit_ts
                OR (n_out.dur = 0 AND n_out.ts = fr.emit_ts)
              )
          )
        JOIN win_nodes n_in
          ON n_in.utid = fr.in_utid
          AND fr.in_ts >= n_in.ts
          AND (
            fr.in_ts < n_in.ts + n_in.dur
            OR (fr.in_ts = n_in.ts + n_in.dur AND fr.in_dur = 0)
          )
        WHERE fr.emit_ts >= {lookback_ts}
          AND n_in.id != n_out.id
        GROUP BY n_in.id, n_out.id, fr.in_ts, fr.emit_ts
        ORDER BY n_in.id ASC, handoff_ts ASC;
        """
    )
    if not flow_rows:
        return [], "unreachable"

    for fr in flow_rows:
        _put_edge(
            _CausalEdge(
                int(fr["source_node_id"]),
                int(fr["dest_node_id"]),
                int(fr["dest_utid"]),
                int(fr["dest_ts"]),
                int(fr["req_ts"]),
                int(fr["dest_latest_ts"]),
                int(fr["handoff_ts"]),
                2,
                0,
                str(fr["edge_kind"]),
            )
        )

    tier2_rows = _search_causal_chain(
        start_ranks=start_ranks,
        end_seeds=end_seeds,
        edges=list(dedup_edges.values()),
        start_ts=start_ep.ts,
        min_causal_ts=min_causal_ts,
        prefer_pure_wakeup=False,
    )
    if tier2_rows:
        segs = _chain_rows_to_spine_segments(tier2_rows, start_ep.ts, end_ep.ts)
        used_flow = any(
            str(r.get("edge_kind", "")).startswith("flow")
            for r in tier2_rows[1:]
        )
        kind = "wakeup+flow" if used_flow else "wakeup"
        return (
            segs,
            f"cross-thread {kind} spine ({max(0, len(segs) - 1)} thread handoff(s))",
        )

    return [], "unreachable"


def _flatten_span_with_slices(
    span_row: dict[str, Any],
    slice_hits: Sequence[dict[str, Any]],
    seg_handoff_reason: str,
) -> list[FlatCriticalPathSpan]:
    """Splits a critical-path `thread_state` span across its innermost overlapping slices.

    Uses an elementary-interval sweep over `{span_start, span_end}` unioned with
    all clipped slice boundaries so summed sub-span durations equal `span_row['dur']`
    to the exact nanosecond regardless of slice nesting or multi-track threads.
    """
    span_ts = int(span_row["ts"])
    span_dur = int(span_row["dur"])
    span_end = span_ts + span_dur
    if span_dur <= 0:
        return []

    common_kwargs: dict[str, Any] = {
        "group_id": int(span_row["group_id"]),
        "seg_index": int(span_row["seg_index"]),
        "utid": int(span_row["utid"]),
        "tid": int(span_row["tid"]) if span_row["tid"] is not None else None,
        "thread_name": span_row["thread_name"],
        "pid": int(span_row["pid"]) if span_row["pid"] is not None else None,
        "process_name": span_row["process_name"],
        "state": str(span_row["state"] or "Unknown"),
        "cpu": int(span_row["cpu"]) if span_row["cpu"] is not None else None,
        "waker_utid": (
            int(span_row["waker_utid"])
            if span_row["waker_utid"] is not None
            else None
        ),
        "waker_thread_name": span_row["waker_thread_name"],
        "waker_process_name": span_row["waker_process_name"],
        "seg_handoff_reason": seg_handoff_reason,
    }

    valid_hits: list[tuple[int, int, int, int, str, str | None]] = []
    points: set[int] = {span_ts, span_end}
    for hit in slice_hits:
        if hit.get("slice_id") is None:
            continue
        clip_start = max(span_ts, int(hit["slice_clip_ts"]))
        clip_end = min(
            span_end, int(hit["slice_clip_ts"]) + int(hit["slice_clip_dur"])
        )
        if clip_end <= clip_start:
            continue
        points.add(clip_start)
        points.add(clip_end)
        valid_hits.append(
            (
                clip_start,
                clip_end,
                int(hit["depth"] or 0),
                int(hit["slice_id"]),
                str(hit["slice_name"] or UNINSTRUMENTED_SLICE),
                hit.get("category"),
            )
        )

    if not valid_hits:
        return [
            FlatCriticalPathSpan(
                ts=span_ts,
                dur=span_dur,
                slice_name=UNINSTRUMENTED_SLICE,
                category=None,
                **common_kwargs,
            )
        ]

    valid_hits.sort(key=lambda h: h[0])
    sorted_pts = sorted(points)
    heap: list[tuple[int, int, int, str, str | None]] = []
    hit_idx = 0
    n_hits = len(valid_hits)
    out: list[FlatCriticalPathSpan] = []
    for idx in range(len(sorted_pts) - 1):
        sub_start = sorted_pts[idx]
        sub_end = sorted_pts[idx + 1]
        if sub_end <= sub_start:
            continue
        while hit_idx < n_hits and valid_hits[hit_idx][0] <= sub_start:
            _, h_end, depth, s_id, s_name, s_cat = valid_hits[hit_idx]
            if h_end >= sub_end:
                heapq.heappush(heap, (-depth, -s_id, h_end, s_name, s_cat))
            hit_idx += 1
        while heap and heap[0][2] < sub_end:
            heapq.heappop(heap)
        if heap:
            _, neg_sid, _, slice_name, category = heap[0]
            chosen_slice_id: int | None = -neg_sid
        else:
            slice_name = UNINSTRUMENTED_SLICE
            category = None
            chosen_slice_id = None
        if (
            out
            and out[-1].slice_id == chosen_slice_id
            and out[-1].slice_name == slice_name
            and out[-1].category == category
            and out[-1].ts + out[-1].dur == sub_start
        ):
            prev = out[-1]
            out[-1] = FlatCriticalPathSpan(
                ts=prev.ts,
                dur=prev.dur + (sub_end - sub_start),
                slice_name=slice_name,
                category=category,
                slice_id=chosen_slice_id,
                **common_kwargs,
            )
        else:
            out.append(
                FlatCriticalPathSpan(
                    ts=sub_start,
                    dur=sub_end - sub_start,
                    slice_name=slice_name,
                    category=category,
                    slice_id=chosen_slice_id,
                    **common_kwargs,
                )
            )
    return out


def _interval_gaps_cte_sql(
    *,
    cte_name: str,
    parent_table: str,
    parent_key: str,
    child_cte: str,
    child_fk: str,
    parent_select_exprs: Sequence[str],
) -> str:
    """Builds a 4-branch `UNION ALL` CTE computing uncovered `[ts, dur)` gaps in `parent_table`.

    Requires `child_cte` to provide non-overlapping child intervals partitioned by
    `child_fk` with columns `(ts, dur, prev_end, rn_asc, rn_desc)`.
    """
    prefix = ", ".join(parent_select_exprs)
    return f"""
      {cte_name} AS (
        SELECT {prefix}, p.ts, p.dur
        FROM {parent_table} p
        LEFT JOIN (SELECT DISTINCT {child_fk} FROM {child_cte}) cov
          ON cov.{child_fk} = p.{parent_key}
        WHERE cov.{child_fk} IS NULL
        UNION ALL
        SELECT {prefix}, p.ts, c.ts - p.ts AS dur
        FROM {child_cte} c
        JOIN {parent_table} p ON p.{parent_key} = c.{child_fk}
        WHERE c.rn_asc = 1 AND c.ts > p.ts
        UNION ALL
        SELECT {prefix}, c.prev_end AS ts, c.ts - c.prev_end AS dur
        FROM {child_cte} c
        JOIN {parent_table} p ON p.{parent_key} = c.{child_fk}
        WHERE c.prev_end IS NOT NULL AND c.ts > c.prev_end
        UNION ALL
        SELECT {prefix}, c.ts + c.dur AS ts, (p.ts + p.dur) - (c.ts + c.dur) AS dur
        FROM {child_cte} c
        JOIN {parent_table} p ON p.{parent_key} = c.{child_fk}
        WHERE c.rn_desc = 1 AND (c.ts + c.dur) < (p.ts + p.dur)
      )"""


def _build_tiled_intervals_table_sql(
    *,
    table_name: str,
    raw_source_sql: str,
    parent_table: str,
    parent_key: str,
    child_fk: str,
    carry_cols: Sequence[str],
    gap_select_exprs: Sequence[str],
    output_cols: Sequence[str],
    id_order_by: str,
    extra_cols: Sequence[str] = (),
    gap_extra_exprs: Sequence[str] = (),
) -> str:
    """Builds a Perfetto table tiling `parent_table` with deduped, clipped children + gaps.

    `raw_source_sql` must expose `({child_fk}, ..., ts, raw_dur, tie_id, *extra_cols)`.
    `with_prev` is `AS MATERIALIZED` so the main `UNION ALL` and 4 gap branches scan once.
    """
    all_carry = [*carry_cols, *extra_cols]
    carry_csv = ", ".join(all_carry)
    extra_suffix = (", " + ", ".join(extra_cols)) if extra_cols else ""
    gap_extra_suffix = (
        (", " + ", ".join(gap_extra_exprs)) if gap_extra_exprs else ""
    )
    out_csv = ", ".join(output_cols)
    gap_out_cols = [c for c in output_cols if c not in extra_cols]
    gap_out_csv = ", ".join(gap_out_cols)
    gaps_cte = _interval_gaps_cte_sql(
        cte_name="gaps",
        parent_table=parent_table,
        parent_key=parent_key,
        child_cte="with_prev",
        child_fk=child_fk,
        parent_select_exprs=gap_select_exprs,
    )
    return f"""
    CREATE OR REPLACE PERFETTO TABLE {table_name} AS
    WITH
      grouped AS (
        SELECT {carry_csv}, ts, raw_dur
        FROM (
          SELECT
            *,
            ROW_NUMBER() OVER (
              PARTITION BY {child_fk}, ts
              ORDER BY raw_dur DESC, tie_id ASC
            ) AS rn
          FROM ({raw_source_sql})
        )
        WHERE rn = 1
      ),
      dedup AS (
        SELECT
          { ", ".join(carry_cols) },
          ts,
          MIN(
            raw_dur,
            COALESCE(
              LEAD(ts) OVER (PARTITION BY {child_fk} ORDER BY ts) - ts,
              raw_dur
            )
          ) AS dur{extra_suffix}
        FROM grouped
      ),
      with_prev AS MATERIALIZED (
        SELECT
          c.*,
          LAG(c.ts + c.dur) OVER (PARTITION BY c.{child_fk} ORDER BY c.ts) AS prev_end,
          ROW_NUMBER() OVER (PARTITION BY c.{child_fk} ORDER BY c.ts) AS rn_asc,
          ROW_NUMBER() OVER (PARTITION BY c.{child_fk} ORDER BY c.ts DESC) AS rn_desc
        FROM dedup c
        WHERE c.dur > 0
      ),
      {gaps_cte}
    SELECT
      ROW_NUMBER() OVER (ORDER BY {id_order_by}) AS id,
      {out_csv}
    FROM (
      SELECT {out_csv} FROM with_prev
      UNION ALL
      SELECT {gap_out_csv}{gap_extra_suffix}
      FROM gaps
      WHERE dur > 0
    );"""


def compute_critical_path(
    tp: PerfettoTraceProcessor,
    db_objects: set[str],
    spine_segments: Sequence[SpineSegment],
    *,
    limit: int = 20,
    timeline_limit: int = 100,
    coalesce_timeline: bool = True,
    materialize_pa_nodes: bool = True,
) -> dict[int, GroupCriticalPathBreakdown]:
    """Computes critical-path thread summaries, top slices, and timelines for `spine_segments`.

    Executes one batched PerfettoSQL script across all groups (`group_id`),
    returning a mapping `group_id -> GroupCriticalPathBreakdown`.
    """
    valid_segments = [s for s in spine_segments if s.dur > 0]
    if not valid_segments:
        return {}

    min_ts = min(s.ts for s in valid_segments)
    max_ts = max(s.ts + s.dur for s in valid_segments)
    group_windows_by_id: dict[int, tuple[int, int]] = {}
    for seg in valid_segments:
        seg_end = seg.ts + seg.dur
        cur_win = group_windows_by_id.get(seg.group_id)
        if cur_win is None:
            group_windows_by_id[seg.group_id] = (seg.ts, seg_end)
        else:
            group_windows_by_id[seg.group_id] = (
                min(cur_win[0], seg.ts),
                max(cur_win[1], seg_end),
            )

    values_rows = ",\n      ".join(
        f"({idx + 1}, {int(s.group_id)}, {int(s.seg_index)}, "
        f"{int(s.utid)}, {int(s.ts)}, {int(s.dur)})"
        for idx, s in enumerate(valid_segments)
    )
    handoff_by_group_seg: dict[tuple[int, int], str] = {
        (s.group_id, s.seg_index): s.handoff_reason for s in valid_segments
    }

    has_process = "process" in db_objects
    has_slices = {"slice", "thread_track"}.issubset(db_objects)

    proc_join = "LEFT JOIN process p ON t.upid = p.upid" if has_process else ""
    proc_cols = (
        "p.pid AS pid, p.name AS process_name,"
        if has_process
        else "NULL AS pid, NULL AS process_name,"
    )
    waker_proc_join = (
        "LEFT JOIN process wp ON wt.upid = wp.upid" if has_process else ""
    )
    waker_proc_col = (
        "wp.name AS waker_process_name"
        if has_process
        else "NULL AS waker_process_name"
    )

    slice_table_sql = (
        f"""
        CREATE OR REPLACE PERFETTO TABLE _pa_scoped_slices AS
        SELECT
          s.id,
          tt.utid,
          s.ts,
          CASE WHEN s.dur = -1 THEN {max_ts} - s.ts ELSE s.dur END AS dur,
          s.depth,
          s.name AS slice_name,
          s.category
        FROM slice s
        JOIN thread_track tt ON s.track_id = tt.id
        JOIN (SELECT DISTINCT utid FROM _pa_cp_all) u ON u.utid = tt.utid
        WHERE s.ts < {max_ts}
          AND (s.dur = -1 OR (s.dur > 0 AND s.ts + s.dur > {min_ts}));
        """
        if has_slices
        else """
        CREATE OR REPLACE PERFETTO TABLE _pa_scoped_slices AS
        SELECT
          0 AS id,
          0 AS utid,
          0 AS ts,
          0 AS dur,
          0 AS depth,
          '' AS slice_name,
          '' AS category
        WHERE 0;
        """
    )

    cp_all_table_sql = _build_tiled_intervals_table_sql(
        table_name="_pa_cp_all",
        raw_source_sql="""
          SELECT
            seg_uid,
            group_id,
            seg_index,
            utid,
            node_id,
            ts,
            dur AS raw_dur,
            node_id AS tie_id
          FROM _pa_cp_raw
        """,
        parent_table="_pa_spine_input",
        parent_key="seg_uid",
        child_fk="seg_uid",
        carry_cols=("seg_uid", "group_id", "seg_index", "utid", "node_id"),
        gap_select_exprs=(
            "p.seg_uid",
            "p.group_id",
            "p.seg_index",
            "p.utid",
            "NULL AS node_id",
        ),
        output_cols=(
            "seg_uid",
            "group_id",
            "seg_index",
            "utid",
            "node_id",
            "ts",
            "dur",
        ),
        id_order_by="group_id, ts, seg_uid",
    )
    cp_ts_spans_table_sql = _build_tiled_intervals_table_sql(
        table_name="_pa_cp_ts_spans",
        raw_source_sql="""
          SELECT
            c.id AS cp_id,
            c.group_id,
            c.seg_index,
            c.utid,
            c.node_id,
            ii.ts,
            ii.dur AS raw_dur,
            st.id AS tie_id,
            st.state,
            st.cpu,
            st.waker_utid
          FROM _interval_intersect!((_pa_cp_all, _pa_scoped_ts), (utid)) ii
          JOIN _pa_cp_all c ON c.id = ii.id_0
          JOIN _pa_scoped_ts st ON st.id = ii.id_1
          WHERE ii.dur > 0
        """,
        parent_table="_pa_cp_all",
        parent_key="id",
        child_fk="cp_id",
        carry_cols=("cp_id", "group_id", "seg_index", "utid", "node_id"),
        gap_select_exprs=(
            "p.id AS cp_id",
            "p.group_id",
            "p.seg_index",
            "p.utid",
            "p.node_id",
        ),
        extra_cols=("state", "cpu", "waker_utid"),
        gap_extra_exprs=(
            "'Unknown' AS state",
            "NULL AS cpu",
            "NULL AS waker_utid",
        ),
        output_cols=(
            "group_id",
            "seg_index",
            "utid",
            "node_id",
            "ts",
            "dur",
            "state",
            "cpu",
            "waker_utid",
        ),
        id_order_by="group_id, ts",
    )

    pa_nodes_ddl = (
        _build_pa_nodes_sql(
            list(group_windows_by_id.values()),
            max_ts,
        )
        if materialize_pa_nodes
        else "INCLUDE PERFETTO MODULE sched.thread_executing_span;"
    )
    query = f"""
    {pa_nodes_ddl}
    INCLUDE PERFETTO MODULE intervals.intersect;

    CREATE OR REPLACE PERFETTO TABLE _pa_spine_input AS
    WITH raw(seg_uid, group_id, seg_index, utid, ts, dur) AS (
      VALUES
      {values_rows}
    )
    SELECT * FROM raw;

    CREATE OR REPLACE PERFETTO TABLE _pa_distinct_spine AS
    SELECT DISTINCT utid, ts, dur FROM _pa_spine_input WHERE dur > 0;

    CREATE OR REPLACE PERFETTO TABLE _pa_cp_raw AS
    SELECT
      si.seg_uid,
      si.group_id,
      si.seg_index,
      cp.utid,
      cp.id AS node_id,
      cp.ts,
      cp.dur
    FROM _critical_path_by_intervals!(_pa_distinct_spine, _pa_nodes) cp
    JOIN _pa_spine_input si
      ON si.utid = cp.root_utid
      AND si.ts = cp.interval_ts
      AND si.dur = cp.interval_dur
    WHERE cp.dur > 0;
    {cp_all_table_sql}

    CREATE OR REPLACE PERFETTO TABLE _pa_scoped_ts AS
    SELECT
      ts_row.id,
      ts_row.utid,
      ts_row.ts,
      CASE WHEN ts_row.dur = -1 THEN {max_ts} - ts_row.ts ELSE ts_row.dur END AS dur,
      ts_row.state,
      ts_row.cpu,
      ts_row.waker_utid
    FROM thread_state ts_row
    JOIN (SELECT DISTINCT utid FROM _pa_cp_all) u USING (utid)
    WHERE ts_row.ts < {max_ts}
      AND (ts_row.dur = -1 OR ts_row.ts + ts_row.dur > {min_ts})
      AND (CASE WHEN ts_row.dur = -1 THEN {max_ts} - ts_row.ts ELSE ts_row.dur END) > 0;
    {cp_ts_spans_table_sql}

    {slice_table_sql}

    WITH
      slice_hits AS (
        SELECT
          ii.id_0 AS span_id,
          ii.ts AS slice_clip_ts,
          ii.dur AS slice_clip_dur,
          ss.id AS slice_id,
          ss.depth,
          ss.slice_name,
          ss.category
        FROM _interval_intersect!((_pa_cp_ts_spans, _pa_scoped_slices), (utid)) ii
        JOIN _pa_scoped_slices ss ON ss.id = ii.id_1
        WHERE ii.dur > 0
      )
    SELECT
      sp.id AS span_id,
      sp.group_id,
      sp.seg_index,
      sp.utid,
      t.tid AS tid,
      t.name AS thread_name,
      {proc_cols}
      sp.ts,
      sp.dur,
      sp.state,
      sp.cpu,
      COALESCE(sp.waker_utid, wn.utid) AS waker_utid,
      wt.name AS waker_thread_name,
      {waker_proc_col},
      sh.slice_id,
      sh.depth,
      sh.slice_name,
      sh.category,
      sh.slice_clip_ts,
      sh.slice_clip_dur
    FROM _pa_cp_ts_spans sp
    LEFT JOIN thread t ON t.utid = sp.utid
    {proc_join}
    LEFT JOIN _pa_nodes n ON n.id = sp.node_id
    LEFT JOIN _pa_nodes wn ON wn.id = n.waker_id
    LEFT JOIN thread wt ON wt.utid = COALESCE(sp.waker_utid, wn.utid)
    {waker_proc_join}
    LEFT JOIN slice_hits sh ON sh.span_id = sp.id
    ORDER BY sp.group_id ASC, sp.ts ASC, sp.id ASC;
    """

    joined_rows = tp.run_query(query)
    if not joined_rows:
        return {}

    # Group joined rows by `span_id` while preserving order.
    spans_by_id: dict[int, dict[str, Any]] = {}
    hits_by_span_id: dict[int, list[dict[str, Any]]] = defaultdict(list)
    for row in joined_rows:
        span_id = int(row["span_id"])
        if span_id not in spans_by_id:
            spans_by_id[span_id] = row
        if row.get("slice_id") is not None:
            hits_by_span_id[span_id].append(row)

    spans_by_group: dict[int, list[FlatCriticalPathSpan]] = defaultdict(list)
    for span_id, span_row in spans_by_id.items():
        grp = int(span_row["group_id"])
        seg_idx = int(span_row["seg_index"])
        seg_reason = handoff_by_group_seg.get((grp, seg_idx), "")
        flat_list = _flatten_span_with_slices(
            span_row, hits_by_span_id.get(span_id, ()), seg_reason
        )
        spans_by_group[grp].extend(flat_list)

    group_bounds: dict[int, tuple[int, int]] = {}
    for seg in valid_segments:
        prev_bounds = group_bounds.get(seg.group_id)
        if prev_bounds is None:
            group_bounds[seg.group_id] = (seg.ts, seg.dur)
        else:
            group_bounds[seg.group_id] = (
                min(prev_bounds[0], seg.ts),
                prev_bounds[1] + seg.dur,
            )

    results: dict[int, GroupCriticalPathBreakdown] = {}
    for group_id, group_spans in spans_by_group.items():
        group_spans.sort(key=lambda s: s.ts)
        start_ts, expected_dur_ns = group_bounds.get(
            group_id,
            (
                group_spans[0].ts,
                sum(s.dur for s in group_spans),
            ),
        )
        total_ns = sum(s.dur for s in group_spans) or expected_dur_ns
        results[group_id] = _aggregate_group_spans(
            group_id=group_id,
            start_ts=start_ts,
            total_dur_ns=total_ns,
            spans=group_spans,
            limit=limit,
            timeline_limit=timeline_limit,
            coalesce_timeline=coalesce_timeline,
        )
    return results


@dataclass
class _TimelineEntry:
    """Accumulator for coalescing contiguous timeline spans before serialization."""

    utid: int
    ts: int
    dur_ns: int
    rel_start_ms: float
    process_name: str | None
    thread_name: str | None
    tid: int | None
    state: str
    cpu: int | None
    slice_name: str
    handoff_from: str

    def to_row(self) -> dict[str, Any]:
        return {
            "rel_start_ms": self.rel_start_ms,
            "ts": self.ts,
            "dur_ms": round(self.dur_ns / 1e6, 3),
            "process_name": self.process_name,
            "thread_name": self.thread_name,
            "tid": self.tid,
            "state": self.state,
            "cpu": self.cpu,
            "slice_name": self.slice_name,
            "handoff_from": self.handoff_from,
        }


def _aggregate_group_spans(
    *,
    group_id: int,
    start_ts: int,
    total_dur_ns: int,
    spans: Sequence[FlatCriticalPathSpan],
    limit: int,
    timeline_limit: int,
    coalesce_timeline: bool,
) -> GroupCriticalPathBreakdown:
    """Aggregates atomic critical-path spans into the 3 standard diagnostic tables."""
    # 1. Process & Thread Summary
    thread_acc: dict[int, dict[str, Any]] = {}
    prev_ts_state: tuple[int, str, int] | None = None
    for sp in spans:
        entry = thread_acc.get(sp.utid)
        if entry is None:
            entry = {
                "process_name": sp.process_name,
                "pid": sp.pid,
                "thread_name": sp.thread_name,
                "tid": sp.tid,
                "total_ns": 0,
                "running_ns": 0,
                "runnable_ns": 0,
                "waking_ns": 0,
                "sleep_ns": 0,
                "unknown_ns": 0,
                "spans": 0,
            }
            thread_acc[sp.utid] = entry
        entry["total_ns"] += sp.dur
        if prev_ts_state != (sp.utid, sp.state, sp.ts):
            entry["spans"] += 1
        prev_ts_state = (sp.utid, sp.state, sp.ts + sp.dur)
        if sp.state == "Running":
            entry["running_ns"] += sp.dur
        elif sp.state in ("R", "R+"):
            entry["runnable_ns"] += sp.dur
        elif sp.state == "W":
            entry["waking_ns"] += sp.dur
        elif sp.state == "Unknown":
            entry["unknown_ns"] += sp.dur
        else:
            entry["sleep_ns"] += sp.dur

    sorted_threads = sorted(
        thread_acc.values(), key=lambda r: int(r["total_ns"]), reverse=True
    )[:limit]
    thread_summary = [
        {
            "process_name": r["process_name"],
            "pid": r["pid"],
            "thread_name": r["thread_name"],
            "tid": r["tid"],
            "critical_path_ms": round(r["total_ns"] / 1e6, 2),
            "pct_of_total": (
                round(100.0 * r["total_ns"] / total_dur_ns, 1)
                if total_dur_ns > 0
                else 0.0
            ),
            "running_ms": round(r["running_ns"] / 1e6, 2),
            "runnable_ms": round(r["runnable_ns"] / 1e6, 2),
            "waking_ms": round(r["waking_ns"] / 1e6, 2),
            "unattributed_sleep_ms": round(r["sleep_ns"] / 1e6, 2),
            "unknown_ms": round(r["unknown_ns"] / 1e6, 2),
            "spans": r["spans"],
        }
        for r in sorted_threads
    ]

    # 2. Top Critical-Path Blocking Slices
    slice_acc: dict[tuple[int, str, str | None], dict[str, Any]] = {}
    prev_uninst_end: dict[tuple[int, str, str | None], int] = {}
    for sp in spans:
        s_key = (sp.utid, sp.slice_name, sp.category)
        s_entry = slice_acc.get(s_key)
        if s_entry is None:
            s_entry = {
                "process_name": sp.process_name,
                "thread_name": sp.thread_name,
                "slice_name": sp.slice_name,
                "category": sp.category,
                "total_ns": 0,
                "running_ns": 0,
                "runnable_waking_ns": 0,
                "occurrences": 0,
                "seen_slice_ids": set(),
            }
            slice_acc[s_key] = s_entry
        s_entry["total_ns"] += sp.dur
        if sp.state == "Running":
            s_entry["running_ns"] += sp.dur
        elif sp.state in ("R", "R+", "W"):
            s_entry["runnable_waking_ns"] += sp.dur
        if sp.slice_id is not None:
            if sp.slice_id not in s_entry["seen_slice_ids"]:
                s_entry["occurrences"] += 1
                s_entry["seen_slice_ids"].add(sp.slice_id)
        else:
            if prev_uninst_end.get(s_key) != sp.ts:
                s_entry["occurrences"] += 1
            prev_uninst_end[s_key] = sp.ts + sp.dur

    sorted_slices = sorted(
        slice_acc.values(), key=lambda r: int(r["total_ns"]), reverse=True
    )[:limit]
    top_slices = [
        {
            "process_name": r["process_name"],
            "thread_name": r["thread_name"],
            "slice_name": r["slice_name"],
            "category": r["category"],
            "critical_path_ms": round(r["total_ns"] / 1e6, 2),
            "pct_of_total": (
                round(100.0 * r["total_ns"] / total_dur_ns, 1)
                if total_dur_ns > 0
                else 0.0
            ),
            "running_ms": round(r["running_ns"] / 1e6, 2),
            "runnable_waking_ms": round(r["runnable_waking_ns"] / 1e6, 2),
            "occurrences": r["occurrences"],
        }
        for r in sorted_slices
    ]

    # 3. Chronological Span Timeline
    merged_spans: list[_TimelineEntry] = []
    prev_utid: int | None = None
    prev_actor: str = ""
    prev_seg_index: int | None = None
    for sp in spans:
        handoff_from = ""
        if prev_utid is not None and sp.utid != prev_utid:
            if (
                sp.waker_utid is not None
                and sp.waker_utid != sp.utid
                and sp.waker_utid == prev_utid
            ):
                handoff_from = f"{prev_actor} (wakeup)"
            elif prev_seg_index != sp.seg_index and sp.seg_handoff_reason:
                if sp.seg_handoff_reason == "wakeup":
                    actor = (
                        _format_actor(
                            sp.waker_process_name,
                            sp.waker_thread_name,
                            sp.waker_utid,
                        )
                        if (
                            sp.waker_utid is not None
                            and sp.waker_utid != sp.utid
                        )
                        else prev_actor
                    )
                    handoff_from = f"{actor} (wakeup)"
                elif sp.seg_handoff_reason not in ("start", "same_thread"):
                    handoff_from = f"{prev_actor} ({sp.seg_handoff_reason})"
                else:
                    handoff_from = f"{prev_actor} (blocked wait)"
            else:
                handoff_from = f"{prev_actor} (blocked wait)"

        if (
            coalesce_timeline
            and merged_spans
            and not handoff_from
            and merged_spans[-1].utid == sp.utid
            and merged_spans[-1].state == sp.state
            and merged_spans[-1].slice_name == sp.slice_name
            and merged_spans[-1].ts + merged_spans[-1].dur_ns == sp.ts
        ):
            merged_spans[-1].dur_ns += sp.dur
        else:
            merged_spans.append(
                _TimelineEntry(
                    utid=sp.utid,
                    ts=sp.ts,
                    dur_ns=sp.dur,
                    rel_start_ms=round((sp.ts - start_ts) / 1e6, 3),
                    process_name=sp.process_name,
                    thread_name=sp.thread_name,
                    tid=sp.tid,
                    state=sp.state,
                    cpu=sp.cpu,
                    slice_name=sp.slice_name,
                    handoff_from=handoff_from,
                )
            )
        prev_utid = sp.utid
        prev_actor = _format_actor(sp.process_name, sp.thread_name, sp.utid)
        prev_seg_index = sp.seg_index

    timeline = [entry.to_row() for entry in merged_spans[:timeline_limit]]

    return GroupCriticalPathBreakdown(
        group_id=group_id,
        start_ts=start_ts,
        total_dur_ns=total_dur_ns,
        thread_summary=thread_summary,
        top_slices=top_slices,
        timeline=timeline,
        total_timeline_spans=len(merged_spans),
    )


def analyze_between_endpoints(
    tp: PerfettoTraceProcessor,
    db_objects: set[str],
    start_spec: EndpointSpec,
    end_spec: EndpointSpec,
    *,
    limit: int = 20,
    timeline_limit: int = 100,
    coalesce_timeline: bool = True,
) -> list[SectionResult]:
    """Resolves two trace endpoints and computes the 3 critical-path `SectionResult`s."""
    section_names = (
        "Critical Path Process & Thread Summary",
        "Critical Path Top Blocking Slices",
        "Critical Path Chronological Span Timeline",
    )
    required_core = {"thread_state", "thread"}
    if missing := required_core - db_objects:
        err = f"Required schema tables/views missing: {', '.join(sorted(missing))}"
        return [SectionResult(name=n, error=err) for n in section_names]

    try:
        start_ep = resolve_endpoint(tp, db_objects, start_spec, role="start")
        end_ep = resolve_endpoint(
            tp, db_objects, end_spec, role="end", min_ts=start_ep.ts
        )
    except EndpointResolutionError as e:
        return [SectionResult(name=n, error=str(e)) for n in section_names]
    except Exception as e:
        err = f"Query execution failed: {e}"
        return [SectionResult(name=n, error=err) for n in section_names]

    if end_ep.ts <= start_ep.ts:
        err = (
            f"End point timestamp ({end_ep.ts} ns) must be greater than start point "
            f"timestamp ({start_ep.ts} ns). Resolved start: {start_ep.describe()}; "
            f"end: {end_ep.describe()}."
        )
        return [SectionResult(name=n, error=err) for n in section_names]

    try:
        spine, mode_desc = find_cross_thread_spine(
            tp, db_objects, start_ep, end_ep
        )
        if not spine:
            err = (
                "No causal scheduler wakeup or trace flow path connects "
                f"start ({start_ep.describe()}) to end ({end_ep.describe()})."
            )
            return [SectionResult(name=n, error=err) for n in section_names]

        breakdowns = compute_critical_path(
            tp,
            db_objects,
            spine,
            limit=limit,
            timeline_limit=timeline_limit,
            coalesce_timeline=coalesce_timeline,
            materialize_pa_nodes=(start_ep.utid == end_ep.utid),
        )
    except Exception as e:
        err = f"Query execution failed: {e}"
        return [SectionResult(name=n, error=err) for n in section_names]

    grp = breakdowns.get(0)
    if grp is None:
        return [SectionResult(name=n, results=[]) for n in section_names]

    window_ms = round((end_ep.ts - start_ep.ts) / 1e6, 2)
    summary_note = (
        f"Start: {start_ep.describe()} -> End: {end_ep.describe()} | "
        f"Window: {window_ms} ms ({mode_desc})"
    )
    timeline_note = (
        f"Showing first {len(grp.timeline)} of {grp.total_timeline_spans} "
        "timeline spans; pass --timeline-limit to show more."
        if grp.total_timeline_spans > len(grp.timeline)
        else None
    )
    return [
        SectionResult(
            name=section_names[0],
            results=grp.thread_summary,
            note=summary_note,
        ),
        SectionResult(
            name=section_names[1],
            results=grp.top_slices,
        ),
        SectionResult(
            name=section_names[2],
            results=grp.timeline,
            note=timeline_note,
        ),
    ]


def _add_endpoint_args(parser: PluginArgumentParser, role: EdgeKind) -> None:
    """Registers the 9 explicit `--{role}-*` CLI flags for `start` or `end`."""
    index_default_desc = (
        "0" if role == "start" else "first match after start timestamp"
    )
    parser.add_argument(
        f"--{role}-slice-id",
        type=int,
        default=None,
        help=f"Slice ID on a thread_track for the {role} point",
    )
    parser.add_argument(
        f"--{role}-slice",
        type=str,
        default=None,
        help=f"Glob pattern matching a thread_track slice name for the {role} point",
    )
    parser.add_argument(
        f"--{role}-process",
        type=str,
        default=None,
        help=f"Optional process name glob filter for the {role} point",
    )
    parser.add_argument(
        f"--{role}-thread",
        type=str,
        default=None,
        help=f"Optional thread name glob filter for the {role} point",
    )
    parser.add_argument(
        f"--{role}-index",
        type=int,
        default=None,
        help=(
            f"0-based (or negative) match index for --{role}-slice "
            f"(default: {index_default_desc})"
        ),
    )
    parser.add_argument(
        f"--{role}-edge",
        choices=["start", "end"],
        default=None,
        help=f"Which boundary of the {role} slice to anchor on (default: {role})",
    )
    parser.add_argument(
        f"--{role}-ts",
        type=int,
        default=None,
        help=(
            f"Explicit {role} timestamp in nanoseconds "
            f"(requires --{role}-tid or --{role}-utid)"
        ),
    )
    parser.add_argument(
        f"--{role}-tid",
        type=int,
        default=None,
        help=f"OS thread ID (tid) for --{role}-ts",
    )
    parser.add_argument(
        f"--{role}-utid",
        type=int,
        default=None,
        help=f"Perfetto unique thread ID (utid) for --{role}-ts",
    )


def parse_cli_endpoint(args: Any, role: EdgeKind) -> EndpointSpec:
    """Validates parsed `--{role}-*` CLI flags and constructs a typed `EndpointSpec`."""
    slice_id: int | None = getattr(args, f"{role}_slice_id")
    slice_glob: str | None = getattr(args, f"{role}_slice")
    process_glob: str | None = getattr(args, f"{role}_process")
    thread_glob: str | None = getattr(args, f"{role}_thread")
    occurrence_index: int | None = getattr(args, f"{role}_index")
    raw_edge: EdgeKind | None = getattr(args, f"{role}_edge")
    ts: int | None = getattr(args, f"{role}_ts")
    tid: int | None = getattr(args, f"{role}_tid")
    utid: int | None = getattr(args, f"{role}_utid")

    has_ts_family = ts is not None or tid is not None or utid is not None
    families_used = sum(
        [
            1 if slice_id is not None else 0,
            1 if slice_glob is not None else 0,
            1 if has_ts_family else 0,
        ]
    )
    if families_used == 0:
        raise PluginArgumentError(
            f"Missing {role} endpoint selector. Provide --{role}-slice-id, "
            f"--{role}-slice, or --{role}-ts with --{role}-tid/--{role}-utid."
        )
    if families_used > 1:
        raise PluginArgumentError(
            f"Conflicting {role} endpoint selectors: choose exactly one of "
            f"--{role}-slice-id, --{role}-slice, or --{role}-ts with --{role}-tid/--{role}-utid."
        )

    edge: EdgeKind = raw_edge if raw_edge is not None else role

    if slice_id is not None:
        if (
            process_glob is not None
            or thread_glob is not None
            or occurrence_index is not None
        ):
            raise PluginArgumentError(
                f"--{role}-process, --{role}-thread, and --{role}-index cannot be "
                f"combined with --{role}-slice-id."
            )
        return SliceIdEndpoint(slice_id=slice_id, edge=edge)

    if slice_glob is not None:
        return SliceGlobEndpoint(
            slice_glob=slice_glob,
            process_glob=process_glob,
            thread_glob=thread_glob,
            occurrence_index=occurrence_index,
            edge=edge,
        )

    if ts is None:
        raise PluginArgumentError(
            f"--{role}-tid/--{role}-utid requires --{role}-ts."
        )
    if tid is not None and utid is not None:
        raise PluginArgumentError(
            f"Specify either --{role}-tid or --{role}-utid, not both."
        )
    if tid is None and utid is None:
        raise PluginArgumentError(
            f"--{role}-ts requires either --{role}-tid or --{role}-utid."
        )
    if occurrence_index is not None:
        raise PluginArgumentError(
            f"--{role}-index is only valid with --{role}-slice."
        )
    if raw_edge is not None:
        raise PluginArgumentError(
            f"--{role}-edge is only valid with --{role}-slice or --{role}-slice-id."
        )

    return ThreadTimestampEndpoint(
        ts=ts,
        tid=tid,
        utid=utid,
        process_glob=process_glob,
        thread_glob=thread_glob,
    )


class CriticalPathPlugin(AnalyzePlugin):
    """Standalone CLI plugin for computing the critical path between two trace points."""

    name: str = "critical-path"
    description: str = (
        "Compute the scheduler wakeup and trace flow critical path between any "
        "two thread-bound points (thread slices or tid/utid + timestamp)"
    )

    @classmethod
    def build_parser(cls) -> PluginArgumentParser:
        parser = PluginArgumentParser(
            prog=f"perf-analyze analyze --plugin {cls.name}"
        )
        _add_endpoint_args(parser, "start")
        _add_endpoint_args(parser, "end")
        parser.add_argument(
            "--limit",
            type=int,
            default=20,
            help="Maximum summary/slice rows to return (default: 20)",
        )
        parser.add_argument(
            "--timeline-limit",
            type=int,
            default=None,
            help="Maximum timeline spans to return (default: max(limit * 5, 100))",
        )
        parser.add_argument(
            "--no-coalesce",
            action="store_false",
            dest="coalesce",
            default=True,
            help="Disable coalescing of adjacent timeline spans with identical thread/state/slice",
        )
        return parser

    def analyze(
        self,
        remaining_args: Sequence[str],
        trace_path: str,
        cache: bool = True,
    ) -> Sequence[SectionResult]:
        parser = self.build_parser()
        args = parser.parse_args(remaining_args)
        if args.limit <= 0:
            raise PluginArgumentError("Limit must be a positive integer.")
        timeline_limit = (
            args.timeline_limit
            if args.timeline_limit is not None
            else max(args.limit * 5, 100)
        )
        if timeline_limit <= 0:
            raise PluginArgumentError(
                "Timeline limit must be a positive integer."
            )

        start_spec = parse_cli_endpoint(args, "start")
        end_spec = parse_cli_endpoint(args, "end")

        with PerfettoTraceProcessor(trace_path, cache=cache) as tp:
            db_objects = tp.get_tables()
            return analyze_between_endpoints(
                tp,
                db_objects,
                start_spec,
                end_spec,
                limit=args.limit,
                timeline_limit=timeline_limit,
                coalesce_timeline=args.coalesce,
            )
