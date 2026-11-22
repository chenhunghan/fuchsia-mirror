# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Isolated Perfetto FXT direct ingestion and parsing backend.

This module handles the direct ingestion of Fuchsia Trace (FXT) files into the
in-memory trace model using PerfettoTraceProcessor.
"""

import dataclasses
import functools
import gc
import sys
import types
from collections import defaultdict
from collections.abc import Iterator, Mapping, Set
from typing import Any, NamedTuple, Self, TypeVar

from tp_shell import PerfettoTraceProcessor
from trace_processing import trace_model, trace_time
from trace_processing.trace_importing import construct_model

_T = TypeVar("_T", bound=tuple[Any, ...])
_SCALAR_ARG_TYPES = frozenset((int, float, str, bool, type(None)))
_MAX_SINGLE_ARG_CACHE_SIZE = 4096


class _CpuRow(NamedTuple):
    cpu: int


class _CpuSchedRow(NamedTuple):
    ts: int
    dur: int
    end_state: str
    priority: int
    tid: int
    state: str


class _ArgRow(NamedTuple):
    arg_set_id: int | None
    key: str
    int_value: int | None
    real_value: float | None
    string_value: str | None


class _SliceRow(NamedTuple):
    id: int
    parent_id: int | None
    arg_set_id: int | None
    ts: int
    dur: int
    category: str
    name: str
    tid: int
    pid: int
    is_async: int


class _CounterRow(NamedTuple):
    ts: int
    value: float
    name: str
    pid: int
    tid: int
    arg_set_id: int | None
    id: int | None


@functools.cache
def _parse_counter_track(track_name: str) -> tuple[str, str, int | None]:
    """Parse a Perfetto counter track name into event name, arg key, and track ID.

    Perfetto's Fuchsia importer names each counter track by joining the event
    name, the counter argument name, and the counter ID with colons
    ("<event_name>:<arg_key>:<counter_id>", e.g. "Rate:CPU:3"). Event names may
    themselves contain colons (e.g. "Demand:Constant:CPU:3"), so the name is
    split from the right. Names that don't have this form are returned whole as
    the event name, with the argument key "value" and no track ID.
    """
    rparts = track_name.rsplit(":", 2)
    if len(rparts) == 3:
        try:
            track_id = int(rparts[2])
        except ValueError:
            pass
        else:
            return sys.intern(rparts[0]), sys.intern(rparts[1]), track_id
    return sys.intern(track_name), "value", None


class _FlowRow(NamedTuple):
    """One row of Perfetto's `flow` table: a hop from `slice_out` to `slice_in`.

    A multi-hop flow A -> B -> C appears as two rows: (A, B) and (B, C), both
    with the same `trace_id`.

    `resolved_out` and `resolved_in` map `slice_out` and `slice_in` onto slices
    that survive the importer's category/pattern filter:
      * the slice itself, if it matches the filter;
      * otherwise its nearest enclosing ancestor slice that matches the filter;
      * otherwise None (no kept slice covers this endpoint).
    Without a filter they are identical to `slice_out` and `slice_in`.

    Ancestors come from Perfetto's `ancestor_slice(id)`, whose `depth` column
    counts down from the root of the slice stack (0 = top-level slice), so
    `ORDER BY depth DESC LIMIT 1` picks the nearest enclosing ancestor.
    """

    slice_out: int | None
    slice_in: int | None
    trace_id: int
    resolved_out: int | None
    resolved_in: int | None


class _ProcessRow(NamedTuple):
    pid: int
    name: str


class _ThreadRow(NamedTuple):
    tid: int
    pid: int
    name: str


def create_model_from_tp_session(
    session: PerfettoTraceProcessor,
    patterns: Set[str] | None = None,
    categories: Set[str] | None = None,
) -> trace_model.Model:
    """Creates a trace model directly from an active PerfettoTraceProcessor session.

    Args:
        session: Active PerfettoTraceProcessor instance.
        patterns: Optional regex patterns to filter events.
        categories: Optional categories to filter events.

    Returns:
        A Model object.
    """

    # Import allocates millions of long-lived objects and creates no garbage
    # cycles worth collecting, so the cyclic GC would repeatedly traverse an
    # ever-growing heap for no benefit. Pause it for the duration of import.
    gc_was_enabled = gc.isenabled()
    gc.disable()
    try:
        importer = _FxtImporter(session, patterns, categories)
        processes = importer.get_pid_to_name()
        threads = importer.get_threads()
        events = importer.get_events()
        scheduling = importer.get_scheduling_records()

        return construct_model(
            processes.pid_to_name,
            threads.tid_to_name,
            threads.tid_to_pid,
            events.events,
            scheduling.records,
        )
    finally:
        if gc_was_enabled:
            gc.enable()


def create_model_from_fxt_path_directly(
    trace_path: str,
    patterns: Set[str] | None = None,
    categories: Set[str] | None = None,
    trace_processor_shell_path: str | None = None,
) -> trace_model.Model:
    """Create a trace model directly from a Perfetto trace file path.

    A temporary PerfettoTraceProcessor session is created for the duration of model creation
    and closed when model creation completes.

    Args:
        trace_path: Path to trace file or URL string.
        patterns: Optional regex patterns to filter events.
        categories: Optional categories to filter events.
        trace_processor_shell_path: Optional path to Perfetto trace_processor_shell executable.

    Returns:
        A trace model containing the trace data.
    """
    with PerfettoTraceProcessor(
        trace_path=trace_path, tp_shell_path=trace_processor_shell_path
    ) as session:
        return create_model_from_tp_session(session, patterns, categories)


@dataclasses.dataclass(frozen=True)
class _EventFilter:
    """Normalized event filter compiled from `(patterns, categories)` arguments.

    `patterns` and `categories` have OR semantics (`trace_metrics.py`): an event
    is kept if its name full-matches `patterns` OR its category is in
    `categories`. Resolving the `None`, `set()`, and `r".*"` edge cases once
    into `include_all` avoids repeating `None`-vs.-empty disambiguation across
    slice, counter, flow, and argument queries.

    TODO(b/587038888): Promote this to a public `EventFilter` type accepted by
    `MetricsProcessor` and the trace importers in place of the split
    `patterns` / `categories` arguments, so callers express "all events",
    "no events", name patterns, and categories directly instead of via `None`,
    `set()`, and `r".*"` sentinels.
    """

    include_all: bool
    name_patterns: frozenset[str]
    categories: frozenset[str]

    @classmethod
    def from_args(
        cls,
        patterns: Set[str] | None,
        categories: Set[str] | None,
    ) -> Self:
        # Import is unfiltered in two cases:
        # 1. Neither `patterns` nor `categories` was specified
        #    (`patterns is None and not categories`).
        # 2. `patterns` contains `r".*"` (e.g. `MetricsProcessor.event_patterns`
        #    default). Under OR semantics, `r".*"` matches every event name and
        #    subsumes any `categories` filter; clearing both avoids running a
        #    no-op `REGEXP` or mistakenly narrowing the import to `categories`.
        if (patterns is None and not categories) or (
            patterns is not None and r".*" in patterns
        ):
            return cls(
                include_all=True,
                name_patterns=frozenset(),
                categories=frozenset(),
            )
        return cls(
            include_all=False,
            name_patterns=frozenset(patterns) if patterns else frozenset(),
            categories=frozenset(categories) if categories else frozenset(),
        )


class _FxtImporter:
    """Encapsulates Perfetto FXT ingestion session state and query parsing."""

    class _Processes(NamedTuple):
        pid_to_name: dict[int, str]

    class _Threads(NamedTuple):
        tid_to_name: dict[int, str]
        tid_to_pid: dict[int, int]

    class _Slices(NamedTuple):
        events: list[trace_model.Event]
        id_to_event: dict[int, trace_model.Event]
        parent_map: dict[int, int]

    class _Counters(NamedTuple):
        events: list[trace_model.Event]

    class _Flows(NamedTuple):
        events: list[trace_model.Event]

    class _SchedulingRecords(NamedTuple):
        records: dict[int, list[trace_model.SchedulingRecord]]

    class _Events(NamedTuple):
        events: list[trace_model.Event]

    def __init__(
        self,
        session: PerfettoTraceProcessor,
        patterns: Set[str] | None = None,
        categories: Set[str] | None = None,
    ) -> None:
        self.session = session
        self._filter = _EventFilter.from_args(patterns, categories)
        self._args_map: (
            Mapping[int, types.MappingProxyType[str, Any]] | None
        ) = None
        # Bounded cache of read-only views for repeated single-entry scalar args
        # (such as kernel:power "Rate" counter events).
        self._single_arg_cache: dict[
            tuple[str, type, Any], types.MappingProxyType[str, Any]
        ] = {}

    def _wrap_args(
        self, arg_dict: Mapping[str, Any]
    ) -> types.MappingProxyType[str, Any]:
        """Wraps `arg_dict` in a read-only view, deduplicating single scalar args."""
        if len(arg_dict) != 1:
            return types.MappingProxyType(arg_dict)
        k, v = next(iter(arg_dict.items()))
        v_type = type(v)
        if v_type not in _SCALAR_ARG_TYPES:
            return types.MappingProxyType(arg_dict)
        cache_key = (k, v_type, v)
        cached = self._single_arg_cache.get(cache_key)
        if cached is not None:
            return cached
        if len(self._single_arg_cache) < _MAX_SINGLE_ARG_CACHE_SIZE:
            proxy = types.MappingProxyType({sys.intern(k): v})
            self._single_arg_cache[cache_key] = proxy
            return proxy
        return types.MappingProxyType(arg_dict)

    def _query_as(self, row_type: type[_T], sql: str) -> Iterator[_T]:
        """Executes a SQL query and yields rows as instances of row_type.

        Validates that row_type fields match the SQL query column names.
        """
        cols, row_iter = self.session.query_rows(sql)
        expected_fields = getattr(row_type, "_fields", None)
        if expected_fields is not None and expected_fields != cols:
            raise ValueError(
                f"Query column mismatch for {row_type.__name__}: "
                f"expected fields {expected_fields}, got columns {cols}"
            )
        make = getattr(row_type, "_make", row_type)
        for raw_row in row_iter:
            yield make(raw_row)

    def get_pid_to_name(self) -> _Processes:
        sql = (
            "SELECT DISTINCT pid, COALESCE(name, '') AS name FROM process ORDER"
            " BY name ASC"
        )
        return self._Processes(
            {row.pid: row.name for row in self._query_as(_ProcessRow, sql)}
        )

    def get_threads(self) -> _Threads:
        sql = """
        SELECT
          tid,
          process.pid AS pid,
          COALESCE(thread.name, '') AS name
        FROM thread
        JOIN process USING (upid)
        ORDER BY name ASC
        """
        raw_tid_to_name: dict[int, str] = {}
        raw_tid_to_pid: dict[int, int] = {}
        for row in self._query_as(_ThreadRow, sql):
            raw_tid_to_name[row.tid] = row.name
            raw_tid_to_pid[row.tid] = row.pid

        tid_to_name = {k: v for k, v in raw_tid_to_name.items() if v}
        tid_to_pid = {k: v for k, v in raw_tid_to_pid.items() if k != v}
        return self._Threads(tid_to_name, tid_to_pid)

    def _get_args_map(self) -> Mapping[int, types.MappingProxyType[str, Any]]:
        if self._args_map is not None:
            return self._args_map

        arg_id_filter_sql = ""
        if not self._filter.include_all:
            arg_id_subqueries: list[str] = []
            slice_filter = self._slice_filter_sql("s")
            if slice_filter not in (None, "0"):
                arg_id_subqueries.append(
                    "SELECT s.arg_set_id FROM slice s "
                    f"WHERE s.arg_set_id IS NOT NULL AND ({slice_filter})"
                )
            counter_filter = self._counter_filter_sql("ct")
            if counter_filter not in (None, "0"):
                arg_id_subqueries.append(
                    "SELECT c.arg_set_id FROM counter c "
                    "JOIN counter_track ct ON c.track_id = ct.id "
                    f"WHERE c.arg_set_id IS NOT NULL AND ({counter_filter})"
                )
            if not arg_id_subqueries:
                self._args_map = {}
                return self._args_map
            union_sql = " UNION ".join(arg_id_subqueries)
            arg_id_filter_sql = f"\n          AND arg_set_id IN ({union_sql})"

        # Filter out serialized binary payloads emitted by Chromium navigation
        # (e.g. debug.params.page_state.data) which contain non-UTF-8 bytes that
        # cause UnicodeDecodeError during protobuf deserialization (b/573890571).
        sql = f"""
        SELECT
          arg_set_id,
          key,
          int_value,
          real_value,
          string_value
        FROM args
        WHERE key NOT LIKE 'debug.params.%.data'{arg_id_filter_sql}
        """
        raw_arg_sets: dict[int, dict[str, Any]] = defaultdict(dict)
        for row in self._query_as(_ArgRow, sql):
            if row.arg_set_id is not None:
                val: Any
                if row.int_value is not None:
                    val = row.int_value
                elif row.real_value is not None:
                    val = row.real_value
                else:
                    val = row.string_value or ""
                raw_arg_sets[row.arg_set_id][sys.intern(row.key)] = val
        # Pre-wrap each arg set in MappingProxyType once so events sharing an
        # arg_set_id reuse the same read-only wrapper without per-event
        # allocations.
        self._args_map = {
            arg_set_id: types.MappingProxyType(arg_dict)
            for arg_set_id, arg_dict in raw_arg_sets.items()
        }
        return self._args_map

    def _slice_filter_sql(self, table_alias: str) -> str | None:
        """Returns a SQL predicate selecting the slices this importer keeps.

        The predicate references columns of the slice table aliased as
        `table_alias`, so the same filter can be applied to the main `slice`
        query and to `ancestor_slice(...)` subqueries. Patterns are anchored to
        enforce full-match semantics (`^(?:...)$`), matching `trace2json`'s
        `RE2::FullMatch`. Returns `None` when unfiltered (`include_all=True`),
        or `"0"` when filtering is active with no matching patterns or
        categories.
        """
        if self._filter.include_all:
            return None
        where_clauses: list[str] = []
        if self._filter.categories:
            cats_joined = ", ".join(
                f"'{c}'" for c in sorted(self._filter.categories)
            )
            where_clauses.append(
                f"COALESCE({table_alias}.category, '') IN ({cats_joined})"
            )
        if self._filter.name_patterns:
            combined_pattern = "|".join(
                f"({p})" for p in sorted(self._filter.name_patterns)
            )
            where_clauses.append(
                f"{table_alias}.name REGEXP '^(?:{combined_pattern})$'"
            )
        return " OR ".join(where_clauses) if where_clauses else "0"

    def _counter_filter_sql(self, table_alias: str) -> str | None:
        """Returns a SQL predicate selecting the counter tracks this importer keeps.

        Returns `None` when unfiltered (`include_all=True`), a full-match
        (`^(?:...)$`) `REGEXP` predicate on the parsed `event_name` in
        `"{event_name}:{arg_key}:{track_id}"` (matching `_parse_counter_track`'s
        right-split so colon-containing names like `"Demand:Constant:CPU:3"`
        preserve `"Demand:Constant"`, and falling back to the full name for
        native Perfetto counters) when `name_patterns` is non-empty, or `"0"`
        otherwise.

        Note: Perfetto's Fuchsia trace parser discards FXT counter categories
        when populating `counter_track` (all imported `CounterEvent`s have
        `category=""`), and `MetricsProcessor.category_names` is used only for
        slice/flow chains while counter processors always declare
        `event_patterns`. Consequently, category-only filters return `"0"` here
        and match no counters.
        """
        if self._filter.include_all:
            return None
        if self._filter.name_patterns:
            combined_pattern = "|".join(
                f"({p})" for p in sorted(self._filter.name_patterns)
            )
            event_name_expr = (
                f"COALESCE(REGEXP_EXTRACT({table_alias}.name,"
                f" '^(.*):[^:]+:-?[0-9]+$'), {table_alias}.name)"
            )
            return f"{event_name_expr} REGEXP '^(?:{combined_pattern})$'"
        return "0"

    def get_slices(self) -> _Slices:
        arg_set_id_to_args = self._get_args_map()
        filter_sql = self._slice_filter_sql("slice")

        if filter_sql is None:
            where_sql = ""
            parent_id_expr = "slice.parent_id"
        elif filter_sql == "0":
            where_sql = " WHERE 0"
            parent_id_expr = "slice.parent_id"
        else:
            ancestor_filter_sql = self._slice_filter_sql("a")
            where_sql = f" WHERE {filter_sql}"
            # A kept slice's direct parent may have been filtered out. Re-parent
            # it onto its nearest enclosing kept ancestor (see `_FlowRow` for
            # Perfetto's `depth` convention), so the restored hierarchy only
            # references kept slices. Zero-duration ancestors are skipped
            # because they become InstantEvents, which cannot be parents in
            # trace_model. If no ancestor qualifies, the slice becomes a root.
            parent_id_expr = f"""
            CASE
              WHEN slice.parent_id IS NULL THEN NULL
              ELSE (
                SELECT a.id
                FROM ancestor_slice(slice.id) a
                WHERE a.dur != 0
                  AND ({ancestor_filter_sql})
                ORDER BY a.depth DESC
                LIMIT 1
              )
            END
            """

        sql = f"""
        SELECT
          slice.id AS id,
          {parent_id_expr} AS parent_id,
          slice.arg_set_id AS arg_set_id,
          COALESCE(slice.ts, 0) AS ts,
          COALESCE(slice.dur, 0) AS dur,
          COALESCE(slice.category, '') AS category,
          COALESCE(slice.name, '') AS name,
          COALESCE(thread.tid, 0) AS tid,
          COALESCE(process.pid, async_process.pid, 0) AS pid,
          (thread_track.id IS NULL) AS is_async
        FROM slice
        LEFT JOIN track ON slice.track_id = track.id
        LEFT JOIN thread_track ON track.id = thread_track.id
        LEFT JOIN thread ON thread_track.utid = thread.utid
        LEFT JOIN process_track ON track.id = process_track.id
        LEFT JOIN process ON COALESCE(thread.upid, process_track.upid) = process.upid
        LEFT JOIN args ON track.source_arg_set_id = args.arg_set_id AND args.key = 'upid'
        LEFT JOIN process async_process ON args.int_value = async_process.upid
        {where_sql}
        """
        slice_events: list[trace_model.Event] = []
        slice_id_to_event: dict[int, trace_model.Event] = {}
        slice_parent_map: dict[int, int] = {}

        for row in self._query_as(_SliceRow, sql):
            dur_ns = row.dur
            arg_id = row.arg_set_id
            args = (
                arg_set_id_to_args.get(arg_id) if arg_id is not None else None
            )
            is_async = bool(row.is_async)
            start = trace_time.TimePoint(row.ts)
            tid = 0 if is_async else row.tid

            event: trace_model.Event
            if is_async:
                event = trace_model.AsyncEvent(
                    id=0,
                    duration=trace_time.TimeDelta(dur_ns),
                    category=row.category,
                    name=row.name,
                    start=start,
                    pid=row.pid,
                    tid=tid,
                    args=args,
                )
            elif dur_ns == 0:
                event = trace_model.InstantEvent(
                    scope=trace_model.InstantEventScope.THREAD,
                    category=row.category,
                    name=row.name,
                    start=start,
                    pid=row.pid,
                    tid=tid,
                    args=args,
                )
            else:
                event = trace_model.DurationEvent(
                    duration=trace_time.TimeDelta(dur_ns),
                    category=row.category,
                    name=row.name,
                    start=start,
                    pid=row.pid,
                    tid=tid,
                    args=args,
                )
            slice_events.append(event)
            slice_id = row.id
            slice_id_to_event[slice_id] = event
            if row.parent_id is not None:
                slice_parent_map[slice_id] = row.parent_id

        return self._Slices(slice_events, slice_id_to_event, slice_parent_map)

    def get_counter_events(self) -> _Counters:
        counter_filter = self._counter_filter_sql("counter_track")
        where_sql = (
            f" WHERE {counter_filter}" if counter_filter is not None else ""
        )

        sql = f"""
        SELECT
          counter.ts AS ts,
          counter.value AS value,
          counter_track.name AS name,
          COALESCE(process.pid, 0) AS pid,
          COALESCE(thread.tid, 0) AS tid,
          counter.arg_set_id AS arg_set_id,
          counter.id AS id
        FROM counter
        JOIN counter_track ON counter.track_id = counter_track.id
        LEFT JOIN process_counter_track ON counter_track.id = process_counter_track.id
        LEFT JOIN process ON process_counter_track.upid = process.upid
        LEFT JOIN thread_counter_track ON counter_track.id = thread_counter_track.id
        LEFT JOIN thread ON thread_counter_track.utid = thread.utid
        {where_sql}
        ORDER BY counter.ts ASC
        """
        arg_set_id_to_args = self._get_args_map()
        counter_events: list[trace_model.Event] = []

        # Rows are ordered by `counter.ts ASC`, so all counter samples sharing a
        # timestamp arrive contiguously. Buffer groups only for `current_ts_ns`
        # and flush finalized CounterEvents as soon as `ts` advances.
        current_ts_ns: int | None = None
        window_groups: dict[
            tuple[int, int, str, int | None], dict[str, Any]
        ] = {}

        def flush_window(ts_ns: int) -> None:
            start = trace_time.TimePoint(ts_ns)
            for (pid, tid, name, track_id), args in window_groups.items():
                counter_events.append(
                    trace_model.CounterEvent(
                        id=track_id if track_id is not None else 0,
                        category="",
                        name=name,
                        start=start,
                        pid=pid,
                        tid=tid,
                        args=self._wrap_args(args),
                    )
                )
            window_groups.clear()

        for row in self._query_as(_CounterRow, sql):
            ts_ns = row.ts
            if current_ts_ns is None:
                current_ts_ns = ts_ns
            elif ts_ns != current_ts_ns:
                flush_window(current_ts_ns)
                current_ts_ns = ts_ns

            event_name, arg_key, track_id = _parse_counter_track(row.name)
            val = float(row.value)
            extra_args = (
                arg_set_id_to_args.get(row.arg_set_id)
                if row.arg_set_id is not None
                else None
            )

            key = (row.pid, row.tid, event_name, track_id)
            group_args = window_groups.get(key)
            if group_args is None:
                group_args = dict(extra_args) if extra_args else {}
                window_groups[key] = group_args
            elif extra_args:
                group_args.update(extra_args)
            group_args[arg_key] = val

        if current_ts_ns is not None:
            flush_window(current_ts_ns)

        return self._Counters(counter_events)

    def get_flow_events(
        self,
        slice_id_to_event: dict[int, trace_model.Event],
        slice_parent_map: dict[int, int],
    ) -> _Flows:
        if not slice_id_to_event:
            return self._Flows([])

        matched_filter_sql = self._slice_filter_sql("m")
        if matched_filter_sql is not None:
            ancestor_filter_sql = self._slice_filter_sql("a")

            # Resolves a flow endpoint column onto a kept slice (see
            # `_FlowRow`): the endpoint itself if it matches the filter,
            # otherwise its nearest enclosing matching ancestor, otherwise NULL.
            def endpoint_sql(col: str) -> str:
                return f"""
                CASE
                  WHEN {col} IS NULL THEN NULL
                  WHEN EXISTS (
                    SELECT 1 FROM slice m WHERE m.id = {col} AND ({matched_filter_sql})
                  ) THEN {col}
                  ELSE (
                    SELECT a.id FROM ancestor_slice({col}) a
                    WHERE {ancestor_filter_sql}
                    ORDER BY a.depth DESC LIMIT 1
                  )
                END
                """

            resolved_out_expr = endpoint_sql("flow.slice_out")
            resolved_in_expr = endpoint_sql("flow.slice_in")
        else:
            resolved_out_expr = "flow.slice_out"
            resolved_in_expr = "flow.slice_in"

        # Hops are processed in `flow.id` order. Carrying a flow across an
        # unkept intermediate slice (see `origin_kept_by_node` below) assumes
        # the hop entering that slice is processed before the hop leaving it.
        # Perfetto assigns flow ids in the order hops are parsed from the
        # trace, which follows the flow's begin/step/end chain order.
        sql = f"""
        SELECT
          flow.slice_out AS slice_out,
          flow.slice_in AS slice_in,
          flow.trace_id AS trace_id,
          {resolved_out_expr} AS resolved_out,
          {resolved_in_expr} AS resolved_in
        FROM flow
        WHERE flow.trace_id IS NOT NULL
          AND (flow.slice_out IS NOT NULL OR flow.slice_in IS NOT NULL)
        ORDER BY flow.id ASC
        """
        flow_events: list[trace_model.Event] = []
        key_to_flow_event: dict[tuple[int, int], trace_model.FlowEvent] = {}
        # Accumulate flows in temporary lists keyed by enclosing DurationEvent
        # and assign the frozen `child_flows` tuples once at the end.
        flows_by_enclosing: dict[
            trace_model.DurationEvent, list[trace_model.FlowEvent]
        ] = defaultdict(list)
        # Maps (unresolved slice_id, flow_trace_id) to the kept slice that the
        # flow originated from before entering that slice. A slice is
        # unresolved when neither it nor any ancestor is kept (its resolved_*
        # value is None). When a later hop leaves the unresolved slice, it
        # continues the flow from this origin.
        origin_kept_by_node: dict[tuple[int, int], int] = {}

        def get_or_create_flow_event(
            slice_id: int, flow_trace_id: int, is_out: bool
        ) -> trace_model.FlowEvent:
            """Retrieves or creates a FlowEvent for a (slice_id, flow_trace_id) pair.

            If the event already exists, updates its phase to STEP if it acts as both
            an incoming and outgoing node in the flow graph.
            """
            key = (slice_id, flow_trace_id)
            if key in key_to_flow_event:
                existing_flow_event = key_to_flow_event[key]
                # Upgrade phase to STEP if an END event is encountered as outgoing.
                if (
                    is_out
                    and existing_flow_event.phase
                    == trace_model.FlowEventPhase.END
                ):
                    existing_flow_event.phase = trace_model.FlowEventPhase.STEP
                return existing_flow_event

            base_event = slice_id_to_event[slice_id]

            # Outgoing slices start as START; incoming slices start as END.
            phase = (
                trace_model.FlowEventPhase.START
                if is_out
                else trace_model.FlowEventPhase.END
            )

            # Associate FlowEvent with its enclosing DurationEvent. If the slice is an
            # InstantEvent (dur == 0) or AsyncEvent (unlikely), walk up the parent chain
            # to locate the enclosing DurationEvent.
            enclosing = None
            if isinstance(base_event, trace_model.DurationEvent):
                enclosing = base_event
            else:
                curr_id = slice_id
                while curr_id in slice_parent_map:
                    curr_id = slice_parent_map[curr_id]
                    parent_event = slice_id_to_event[curr_id]
                    if isinstance(parent_event, trace_model.DurationEvent):
                        enclosing = parent_event
                        break

            flow_event = trace_model.FlowEvent(
                id=str(flow_trace_id),
                phase=phase,
                enclosing_duration=enclosing,
                previous_flow=None,
                next_flow=None,
                category=base_event.category,
                name=base_event.name,
                start=base_event.start,
                pid=base_event.pid,
                tid=base_event.tid,
                args=base_event.args,
            )
            if enclosing is not None:
                flows_by_enclosing[enclosing].append(flow_event)

            key_to_flow_event[key] = flow_event
            flow_events.append(flow_event)
            return flow_event

        # Build FlowEvent chains between kept slices, contracting hops through
        # unkept slices. For example, with Read and WriteSubEnd kept but
        # ReadSubBegin, WriteSubBegin, and Write filtered out, the hops
        #   ReadSubBegin -> WriteSubBegin -> WriteSubEnd
        # become the single kept edge Read -> WriteSubEnd:
        #   * Hop 1 resolves to (Read, NULL): ReadSubBegin resolves to its kept
        #     parent Read, and WriteSubBegin has no kept slice. Record that the
        #     flow reached WriteSubBegin from Read.
        #   * Hop 2 resolves to (NULL, WriteSubEnd). It picks up Read from
        #     `origin_kept_by_node` and links Read -> WriteSubEnd.
        for row in self._query_as(_FlowRow, sql):
            s_out = row.slice_out
            s_in = row.slice_in
            flow_trace_id = row.trace_id
            resolved_out = row.resolved_out
            resolved_in = row.resolved_in

            # The kept slice this hop effectively starts from: its own resolved
            # source if any, otherwise the origin carried in by an earlier hop
            # that ended at this (unresolved) source slice.
            if resolved_out is not None:
                effective_out: int | None = resolved_out
            elif s_out is not None:
                effective_out = origin_kept_by_node.get((s_out, flow_trace_id))
            else:
                effective_out = None

            if effective_out is not None:
                if resolved_in is not None:
                    # Only materialize FlowEvents when a hop connects two distinct
                    # kept slices without forming a cycle or overwriting an existing
                    # outgoing link. Ancestor resolution can map multiple filtered
                    # child slices under the same kept parent to the same slice_id
                    # (e.g., A_sub1 -> B -> A_sub2 -> C becomes A -> B -> A -> C),
                    # which would otherwise create a cycle or second outgoing edge.
                    existing_out = key_to_flow_event.get(
                        (effective_out, flow_trace_id)
                    )
                    if (
                        effective_out != resolved_in
                        and (
                            existing_out is None
                            or existing_out.next_flow is None
                        )
                        and (resolved_in, flow_trace_id)
                        not in key_to_flow_event
                    ):
                        flow_event_out = get_or_create_flow_event(
                            effective_out, flow_trace_id, is_out=True
                        )
                        flow_event_in = get_or_create_flow_event(
                            resolved_in, flow_trace_id, is_out=False
                        )
                        flow_event_out.next_flow = flow_event_in
                        flow_event_in.previous_flow = flow_event_out
                elif s_in is not None:
                    # The destination has no kept slice; remember where the
                    # flow came from so the hop leaving `s_in` can continue it.
                    origin_kept_by_node[(s_in, flow_trace_id)] = effective_out

        for enclosing_event, attached_flows in flows_by_enclosing.items():
            enclosing_event.child_flows = attached_flows

        return self._Flows(flow_events)

    def get_scheduling_records(
        self,
    ) -> _SchedulingRecords:
        cpu_sql = """
        SELECT DISTINCT cpu
        FROM (
          SELECT COALESCE(s.cpu, 0) AS cpu FROM sched s
          UNION
          SELECT COALESCE(ts.cpu, ts.ucpu, 0) AS cpu FROM thread_state ts WHERE ts.state = 'W'
        )
        ORDER BY cpu ASC
        """
        scheduling_records: dict[
            int, list[trace_model.SchedulingRecord]
        ] = defaultdict(list)

        END_STATE_MAP: dict[str, trace_model.ThreadState] = {
            "R": trace_model.ThreadState.ZX_THREAD_STATE_RUNNING,
            "S": trace_model.ThreadState.ZX_THREAD_STATE_BLOCKED,
            "D": trace_model.ThreadState.ZX_THREAD_STATE_BLOCKED,
            "T": trace_model.ThreadState.ZX_THREAD_STATE_SUSPENDED,
            "Z": trace_model.ThreadState.ZX_THREAD_STATE_DYING,
            "X": trace_model.ThreadState.ZX_THREAD_STATE_DEAD,
        }
        IDLE_TID = 0
        IDLE_PRIO = trace_model.INT32_MIN
        DEFAULT_RUNNING_STATE = END_STATE_MAP["R"]

        for cpu_row in self._query_as(_CpuRow, cpu_sql):
            cpu = cpu_row.cpu
            records = scheduling_records[cpu]
            last_end_ts: int | None = None
            last_tid = IDLE_TID
            last_prio: int | None = None
            last_end_state = DEFAULT_RUNNING_STATE

            sql = f"""
            WITH cs AS (
              SELECT
                s.ts AS ts,
                COALESCE(s.dur, 0) AS dur,
                COALESCE(s.end_state, '') AS end_state,
                COALESCE(s.priority, 0) AS priority,
                COALESCE(t.tid, 0) AS tid,
                'Running' AS state
              FROM sched s
              LEFT JOIN thread t ON s.utid = t.utid
              WHERE COALESCE(s.cpu, 0) = {cpu}
            ),
            waking AS (
              SELECT
                ts.ts AS ts,
                0 AS dur,
                '' AS end_state,
                0 AS priority,
                COALESCE(t.tid, 0) AS tid,
                'W' AS state
              FROM thread_state ts
              JOIN thread t ON ts.utid = t.utid
              WHERE ts.state = 'W' AND COALESCE(ts.cpu, ts.ucpu, 0) = {cpu}
            )
            SELECT ts, dur, end_state, priority, tid, state
            FROM (
              SELECT * FROM cs
              UNION ALL
              SELECT * FROM waking
              ORDER BY ts
            )
            """

            for row in self._query_as(_CpuSchedRow, sql):
                ts_ns = row.ts
                timestamp = trace_time.TimePoint(ts_ns)

                if (
                    last_end_ts is not None
                    and ts_ns > last_end_ts
                    and last_tid != IDLE_TID
                ):
                    records.append(
                        trace_model.ContextSwitch(
                            start=trace_time.TimePoint(last_end_ts),
                            incoming_tid=IDLE_TID,
                            outgoing_tid=last_tid,
                            incoming_prio=IDLE_PRIO,
                            outgoing_prio=last_prio,
                            outgoing_state=last_end_state,
                        )
                    )
                    last_tid = IDLE_TID
                    last_prio = IDLE_PRIO
                    last_end_state = DEFAULT_RUNNING_STATE

                if row.state == "W":
                    records.append(
                        trace_model.Waking(
                            start=timestamp,
                            tid=row.tid,
                            prio=0,
                        )
                    )
                    continue

                raw_dur = row.dur
                tid = row.tid
                prio = row.priority

                records.append(
                    trace_model.ContextSwitch(
                        start=timestamp,
                        incoming_tid=tid,
                        outgoing_tid=last_tid,
                        incoming_prio=prio,
                        outgoing_prio=last_prio,
                        outgoing_state=last_end_state,
                    )
                )

                last_end_ts = ts_ns + raw_dur if raw_dur >= 0 else None
                last_tid = tid
                last_prio = prio
                last_end_state = END_STATE_MAP.get(
                    row.end_state, DEFAULT_RUNNING_STATE
                )

            if last_end_ts is not None and last_tid != IDLE_TID:
                records.append(
                    trace_model.ContextSwitch(
                        start=trace_time.TimePoint(last_end_ts),
                        incoming_tid=IDLE_TID,
                        outgoing_tid=last_tid,
                        incoming_prio=IDLE_PRIO,
                        outgoing_prio=last_prio,
                        outgoing_state=last_end_state,
                    )
                )

        return self._SchedulingRecords(scheduling_records)

    def _restore_slice_hierarchy(
        self,
        slice_parent_map: dict[int, int],
        slice_id_to_event: dict[int, trace_model.Event],
    ) -> None:
        """Restores parent-child relationships between DurationEvents in-place.

        Perfetto's `slice` table represents slice nesting via `parent_id` foreign
        keys. However, `trace_model.DurationEvent` objects rely on explicit object-level
        tree relationships (`child.parent` references and `parent_event.child_durations`
        tuples) so that metrics processors and model utilities can traverse nested duration
        stacks and sub-events.

        This method iterates over the slice parent mappings to construct those
        bidirectional parent-child links between instanced DurationEvent objects.
        """
        # Accumulate child durations in temporary lists keyed by parent
        # DurationEvent and assign the frozen `child_durations` tuples once at
        # the end.
        children_by_parent: dict[
            trace_model.DurationEvent, list[trace_model.DurationEvent]
        ] = defaultdict(list)
        for slice_id, parent_id in slice_parent_map.items():
            if slice_id in slice_id_to_event and parent_id in slice_id_to_event:
                child = slice_id_to_event[slice_id]
                parent_event = slice_id_to_event[parent_id]
                if isinstance(child, trace_model.DurationEvent) and isinstance(
                    parent_event, trace_model.DurationEvent
                ):
                    child.parent = parent_event
                    children_by_parent[parent_event].append(child)

        for parent_event, children in children_by_parent.items():
            parent_event.child_durations = children

    def get_events(self) -> _Events:
        slices = self.get_slices()
        counters = self.get_counter_events()
        flows = self.get_flow_events(
            slices.id_to_event,
            slices.parent_map,
        )

        result_events: list[trace_model.Event] = (
            slices.events + counters.events + flows.events
        )

        self._restore_slice_hierarchy(slices.parent_map, slices.id_to_event)

        result_events.sort(
            key=lambda e: (
                e.start.to_epoch_nanoseconds(),
                0
                if isinstance(e, trace_model.DurationEvent)
                and e.duration is not None
                else (
                    1
                    if isinstance(e, trace_model.FlowEvent)
                    else (2 if isinstance(e, trace_model.DurationEvent) else 3)
                ),
            )
        )

        return self._Events(result_events)
