# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for trace_importing_fxt.py."""

import contextlib
import gc
import json
import pathlib
import sys
import tempfile
import types
import unittest
from collections.abc import Mapping, Sequence
from typing import Any
from unittest import mock

import test_utils
from tp_shell import PerfettoTraceProcessor
from trace_processing import (
    trace_importing_fxt,
    trace_model,
    trace_time,
)

# Shorthands for building expected scheduling records.
_IDLE = trace_model.INT32_MIN  # Priority of the idle thread.
_RUNNING = trace_model.ThreadState.ZX_THREAD_STATE_RUNNING
_BLOCKED = trace_model.ThreadState.ZX_THREAD_STATE_BLOCKED
_SUSPENDED = trace_model.ThreadState.ZX_THREAD_STATE_SUSPENDED
_DYING = trace_model.ThreadState.ZX_THREAD_STATE_DYING
_DEAD = trace_model.ThreadState.ZX_THREAD_STATE_DEAD


class QueriesTest(unittest.TestCase):
    """Dedicated unit test suite validating trace query helper methods on _FxtImporter."""

    _exit_stack: contextlib.ExitStack
    session: PerfettoTraceProcessor
    importer: trace_importing_fxt._FxtImporter

    @classmethod
    def setUpClass(cls) -> None:
        cls._exit_stack = contextlib.ExitStack()
        fxt_path = (
            pathlib.Path(__file__).resolve().parent.parent.parent
            / "runtime_deps"
            / "model.fxt"
        )
        cls.session = cls._exit_stack.enter_context(
            PerfettoTraceProcessor(trace_path=str(fxt_path))
        )
        cls.importer = trace_importing_fxt._FxtImporter(cls.session)

    @classmethod
    def tearDownClass(cls) -> None:
        cls._exit_stack.close()

    def test_query_as(self) -> None:
        from typing import NamedTuple

        class TestRow(NamedTuple):
            val: int
            label: str

        rows = list(
            self.importer._query_as(
                TestRow, "SELECT 123 AS val, 'sample' AS label"
            )
        )
        self.assertEqual(len(rows), 1)
        self.assertEqual(rows[0].val, 123)
        self.assertEqual(rows[0].label, "sample")
        self.assertIsInstance(rows[0], TestRow)

        # Mismatched columns must raise ValueError
        with self.assertRaises(ValueError):
            list(
                self.importer._query_as(
                    TestRow, "SELECT 123 AS wrong_col, 'sample' AS label"
                )
            )

    def test_legacy_query_method_removed(self) -> None:
        self.assertFalse(
            hasattr(trace_importing_fxt._FxtImporter, "_query"),
            "_query must be deleted in favor of typed _query_as",
        )

    def test_get_slices_with_filtering(self) -> None:
        # 1. Filter by pattern uses full-match semantics: "Read" matches "Read"
        # but not "ReadSubBegin", while "Read.*" matches both.
        importer_filtered = trace_importing_fxt._FxtImporter(
            self.session, patterns={"Read"}
        )
        events, _, _ = importer_filtered.get_slices()
        self.assertGreater(len(events), 0)
        for event in events:
            self.assertEqual(event.name, "Read")

        importer_prefix = trace_importing_fxt._FxtImporter(
            self.session, patterns={"Read.*"}
        )
        prefix_events, _, _ = importer_prefix.get_slices()
        self.assertIn("ReadSubBegin", {e.name for e in prefix_events})

        # 2. Filter by category (both with patterns=None and patterns=set())
        all_events, _, _ = self.importer.get_slices()
        target_cat = all_events[0].category
        patterns_opts: tuple[set[str] | None, ...] = (None, set())
        for patterns_arg in patterns_opts:
            importer_cat = trace_importing_fxt._FxtImporter(
                self.session, patterns=patterns_arg, categories={target_cat}
            )
            cat_events, _, _ = importer_cat.get_slices()
            self.assertGreater(len(cat_events), 0)
            for event in cat_events:
                self.assertEqual(event.category, target_cat)

        # 3. Empty pattern set with no categories discards all events.
        importer_empty = trace_importing_fxt._FxtImporter(
            self.session, patterns=set()
        )
        empty_events, _, _ = importer_empty.get_slices()
        self.assertEqual(len(empty_events), 0)

        # 4. Wildcard pattern r".*" short-circuits filtering (including when
        # combined with categories under OR semantics).
        importer_wildcard = trace_importing_fxt._FxtImporter(
            self.session, patterns={r".*"}, categories={target_cat}
        )
        self.assertTrue(importer_wildcard._filter.include_all)
        self.assertEqual(importer_wildcard._filter.name_patterns, frozenset())
        self.assertEqual(importer_wildcard._filter.categories, frozenset())
        wildcard_events, _, _ = importer_wildcard.get_slices()
        self.assertEqual(len(wildcard_events), len(all_events))
        all_counters = self.importer.get_counter_events().events
        self.assertGreater(len(all_counters), 0)
        self.assertEqual(
            len(importer_wildcard.get_counter_events().events),
            len(all_counters),
        )

    def test_get_pid_to_name(self) -> None:
        processes = self.importer.get_pid_to_name()
        # Verify process mapping resolved actual Fuchsia processes from trace
        self.assertGreater(len(processes.pid_to_name), 0)
        self.assertIn("process_foo", processes.pid_to_name.values())

    def test_get_threads(self) -> None:
        tid_to_name, tid_to_pid = self.importer.get_threads()
        # Verify thread names mapped correctly
        self.assertGreater(len(tid_to_name), 0)
        self.assertIn("initial-thread", tid_to_name.values())

    def test_get_args_map(self) -> None:
        arg_set_id_to_args = self.importer._get_args_map()
        self.assertGreater(len(arg_set_id_to_args), 0)
        found_bool = False
        for args in arg_set_id_to_args.values():
            if "trace_id_is_process_scoped" in args:
                val = args["trace_id_is_process_scoped"]
                self.assertEqual(val, 0)
                found_bool = True
        self.assertTrue(found_bool)

    def test_get_args_map_excludes_chromium_binary_data(self) -> None:
        fresh_importer = trace_importing_fxt._FxtImporter(self.session)
        with mock.patch.object(
            fresh_importer, "_query_as", wraps=fresh_importer._query_as
        ) as mock_query_as:
            args_map = fresh_importer._get_args_map()

        mock_query_as.assert_called_once()
        row_type, executed_sql = mock_query_as.call_args[0]
        self.assertIs(row_type, trace_importing_fxt._ArgRow)
        self.assertIn("WHERE key NOT LIKE 'debug.params.%.data'", executed_sql)
        self.assertGreater(len(args_map), 0)

        # Verify SQLite filter semantics on Perfetto engine
        filtered = self.session.run_query(
            "SELECT 'debug.params.page_state.data' AS key "
            "WHERE 'debug.params.page_state.data' NOT LIKE 'debug.params.%.data'"
        )
        self.assertEqual(len(filtered), 0)

        unfiltered = self.session.run_query(
            "SELECT 'trace_id_is_process_scoped' AS key "
            "WHERE 'trace_id_is_process_scoped' NOT LIKE 'debug.params.%.data'"
        )
        self.assertEqual(len(unfiltered), 1)

    def test_get_slices(self) -> None:
        fresh_importer = trace_importing_fxt._FxtImporter(self.session)
        with mock.patch.object(
            fresh_importer, "_query_as", wraps=fresh_importer._query_as
        ) as mock_query_as:
            (
                result_events,
                slice_id_to_event,
                slice_parent_map,
            ) = fresh_importer.get_slices()

        mock_query_as.assert_called()
        slice_calls = [
            c
            for c in mock_query_as.call_args_list
            if c[0][0] is trace_importing_fxt._SliceRow
        ]
        self.assertEqual(len(slice_calls), 1)

        fresh_importer._restore_slice_hierarchy(
            slice_parent_map, slice_id_to_event
        )
        self.assertGreater(len(result_events), 0)
        slice_names = [e.name for e in result_events]
        self.assertIn("Read", slice_names)

        async_events = [
            e for e in result_events if isinstance(e, trace_model.AsyncEvent)
        ]
        self.assertGreater(len(async_events), 0)
        self.assertIsInstance(async_events[0], trace_model.AsyncEvent)

    def test_get_counter_events(self) -> None:
        fresh_importer = trace_importing_fxt._FxtImporter(self.session)
        with mock.patch.object(
            fresh_importer, "_query_as", wraps=fresh_importer._query_as
        ) as mock_query_as:
            counters = fresh_importer.get_counter_events()

        counter_calls = [
            c
            for c in mock_query_as.call_args_list
            if c[0][0] is trace_importing_fxt._CounterRow
        ]
        self.assertEqual(len(counter_calls), 1)

        self.assertGreater(len(counters.events), 0)
        self.assertIsInstance(counters.events[0], trace_model.CounterEvent)
        self.assertEqual(counters.events[0].name, "cpu_usage")

    def test_get_args_map_scoping_and_prewrapped_proxies(self) -> None:
        all_args = self.importer._get_args_map()
        self.assertGreater(len(all_args), 0)
        for arg_proxy in all_args.values():
            self.assertIsInstance(arg_proxy, types.MappingProxyType)
            for key in arg_proxy:
                self.assertIs(key, sys.intern(key))

        # Empty pattern set with no categories short-circuits without querying SQL.
        empty_importer = trace_importing_fxt._FxtImporter(
            self.session, patterns=set()
        )
        with mock.patch.object(
            empty_importer, "_query_as", wraps=empty_importer._query_as
        ) as mock_empty_query:
            self.assertEqual(empty_importer._get_args_map(), {})
        mock_empty_query.assert_not_called()

        # Filtered importer restricts args SQL via `arg_set_id IN (...)` (keeping
        # `global_instant`'s arg set while excluding `process_instant`,
        # `thread_instant`, and flow/track arg sets) and preserves slice/counter
        # `.args` identically to unfiltered import.
        filtered_importer = trace_importing_fxt._FxtImporter(
            self.session, patterns={"^global_instant$", "^Read$", "cpu_usage"}
        )
        with mock.patch.object(
            filtered_importer, "_query_as", wraps=filtered_importer._query_as
        ) as mock_filtered_query:
            filtered_args = filtered_importer._get_args_map()
        mock_filtered_query.assert_called_once()
        _, executed_sql = mock_filtered_query.call_args[0]
        self.assertIn("AND arg_set_id IN (", executed_sql)
        self.assertGreater(len(filtered_args), 0)
        self.assertLess(len(filtered_args), len(all_args))
        for arg_set_id, proxy in filtered_args.items():
            self.assertEqual(proxy, all_args[arg_set_id])

        unfiltered_events_by_key = {
            (type(e), e.category, e.name, e.start, e.pid, e.tid): e
            for e in (
                *self.importer.get_slices().events,
                *self.importer.get_counter_events().events,
            )
        }
        filtered_slices = filtered_importer.get_slices().events
        filtered_counters = filtered_importer.get_counter_events().events
        self.assertGreater(len(filtered_slices), 0)
        self.assertGreater(len(filtered_counters), 0)
        self.assertTrue(any(bool(e.args) for e in filtered_slices))
        self.assertTrue(any(bool(e.args) for e in filtered_counters))

        filtered_proxies = tuple(filtered_args.values())
        for event in (*filtered_slices, *filtered_counters):
            event_key = (
                type(event),
                event.category,
                event.name,
                event.start,
                event.pid,
                event.tid,
            )
            self.assertIn(event_key, unfiltered_events_by_key)
            self.assertEqual(
                event.args, unfiltered_events_by_key[event_key].args
            )
        # Slices reuse pre-wrapped MappingProxyType instances from the args map.
        # Counter events merge metric values from `counter.value` into separate
        # mappings rather than reusing `_args_map` entries directly.
        for slice_event in filtered_slices:
            if slice_event.args:
                self.assertTrue(
                    any(slice_event.args is proxy for proxy in filtered_proxies)
                )

    def test_get_counter_events_with_filtering(self) -> None:
        # Empty pattern set returns no counter events.
        empty_importer = trace_importing_fxt._FxtImporter(
            self.session, patterns=set()
        )
        self.assertEqual(empty_importer.get_counter_events().events, [])

        # Non-empty pattern set keeps matching counter tracks (`cpu_usage`) and
        # excludes non-matching tracks.
        matching_importer = trace_importing_fxt._FxtImporter(
            self.session, patterns={"cpu_usage"}
        )
        matching_events = matching_importer.get_counter_events().events
        self.assertGreater(len(matching_events), 0)
        self.assertEqual(
            [event.name for event in matching_events],
            ["cpu_usage"] * len(matching_events),
        )

        # Partial substring pattern ("cpu") and arg_key suffix pattern
        # (".*percentage") do not match "cpu_usage:average_cpu_percentage:0"
        # under event_name full-match anchoring, whereas "cpu.*" and explicit
        # "^cpu_usage$" do.
        for non_matching_pat in ("cpu", ".*percentage"):
            partial_importer = trace_importing_fxt._FxtImporter(
                self.session, patterns={non_matching_pat}
            )
            self.assertEqual(partial_importer.get_counter_events().events, [])

        for matching_pat in ("cpu.*", "^cpu_usage$"):
            regex_importer = trace_importing_fxt._FxtImporter(
                self.session, patterns={matching_pat}
            )
            self.assertEqual(
                [
                    event.name
                    for event in regex_importer.get_counter_events().events
                ],
                ["cpu_usage"] * len(matching_events),
            )

        # Category-only filter with empty patterns returns no counter events.
        category_only_importer = trace_importing_fxt._FxtImporter(
            self.session, patterns=set(), categories={"io"}
        )
        self.assertEqual(category_only_importer.get_counter_events().events, [])

        # Colon-containing counter event names (e.g. "Demand:Constant:CPU:3")
        # are right-split consistently with `_parse_counter_track`.
        colon_importer = trace_importing_fxt._FxtImporter(
            self.session, patterns={"Demand:Constant"}
        )
        colon_filter = colon_importer._counter_filter_sql("ct")
        assert colon_filter is not None
        matched_tracks = tuple(
            row["name"]
            for row in self.session.run_query(
                "WITH ct(name) AS (VALUES"
                " ('Demand:Constant:CPU:3'),"
                " ('Demand:Variable:CPU:3'),"
                " ('Demand:CPU:3'))"
                f" SELECT ct.name FROM ct WHERE {colon_filter}"
            )
        )
        self.assertSequenceEqual(matched_tracks, ("Demand:Constant:CPU:3",))

        non_matching_importer = trace_importing_fxt._FxtImporter(
            self.session, patterns={"^Read$"}
        )
        self.assertEqual(non_matching_importer.get_counter_events().events, [])

    def test_get_counter_events_streaming_grouping_and_track_cache(
        self,
    ) -> None:
        fresh_importer = trace_importing_fxt._FxtImporter(self.session)
        fresh_importer._args_map = {
            10: types.MappingProxyType({"unit": "bytes", "zram_bytes": -1.0}),
            11: types.MappingProxyType({"source": "kernel"}),
        }
        mock_rows = (
            # ts=1000: two metrics on track_id=0 merge into one CounterEvent
            # (including merging `extra_args` from both rows); track_id=1 stays
            # separate; plain track without ':' uses "value".
            trace_importing_fxt._CounterRow(
                1000, 128.0, "kmem_stats_a:zram_bytes:0", 1, 2, 10, 100
            ),
            trace_importing_fxt._CounterRow(
                1000, 256.0, "kmem_stats_a:free_bytes:0", 1, 2, 11, 101
            ),
            trace_importing_fxt._CounterRow(
                1000, 512.0, "kmem_stats_a:zram_bytes:1", 1, 2, None, 102
            ),
            trace_importing_fxt._CounterRow(
                1000, 0.75, "cpu_load", 0, 0, None, 103
            ),
            # ts=2000: repeated track names hit parsed_tracks cache and flush a
            # separate timestamp window.
            trace_importing_fxt._CounterRow(
                2000, 64.0, "kmem_stats_a:zram_bytes:0", 1, 2, None, 104
            ),
            trace_importing_fxt._CounterRow(
                2000, 0.5, "cpu_load", 0, 0, None, 105
            ),
            # ts=3000: repeated single-entry scalar value reuses the cached
            # MappingProxyType instance from ts=1000.
            trace_importing_fxt._CounterRow(
                3000, 0.75, "cpu_load", 0, 0, None, 106
            ),
        )
        with mock.patch.object(
            fresh_importer, "_query_as", return_value=iter(mock_rows)
        ):
            events = fresh_importer.get_counter_events().events

        expected: list[trace_model.Event] = [
            trace_model.CounterEvent(
                0,
                category="",
                name="kmem_stats_a",
                start=trace_time.TimePoint(1000),
                pid=1,
                tid=2,
                args={
                    "unit": "bytes",
                    "source": "kernel",
                    "zram_bytes": 128.0,
                    "free_bytes": 256.0,
                },
            ),
            trace_model.CounterEvent(
                1,
                category="",
                name="kmem_stats_a",
                start=trace_time.TimePoint(1000),
                pid=1,
                tid=2,
                args={"zram_bytes": 512.0},
            ),
            trace_model.CounterEvent(
                0,
                category="",
                name="cpu_load",
                start=trace_time.TimePoint(1000),
                pid=0,
                tid=0,
                args={"value": 0.75},
            ),
            trace_model.CounterEvent(
                0,
                category="",
                name="kmem_stats_a",
                start=trace_time.TimePoint(2000),
                pid=1,
                tid=2,
                args={"zram_bytes": 64.0},
            ),
            trace_model.CounterEvent(
                0,
                category="",
                name="cpu_load",
                start=trace_time.TimePoint(2000),
                pid=0,
                tid=0,
                args={"value": 0.5},
            ),
            trace_model.CounterEvent(
                0,
                category="",
                name="cpu_load",
                start=trace_time.TimePoint(3000),
                pid=0,
                tid=0,
                args={"value": 0.75},
            ),
        ]
        test_utils.assertEventListsEqual(self, events, expected)
        self.assertIs(events[2].args, events[5].args)
        self.assertIsNot(events[2].args, events[4].args)

    def test_wrap_args_type_discrimination_and_cache_limit(self) -> None:
        fresh_importer = trace_importing_fxt._FxtImporter(self.session)

        # 1. Numerically equal values of distinct scalar types (int, float, bool)
        # must not collide in `_single_arg_cache`.
        proxy_int = fresh_importer._wrap_args({"v": 0})
        proxy_float = fresh_importer._wrap_args({"v": 0.0})
        proxy_bool = fresh_importer._wrap_args({"v": False})

        self.assertIs(proxy_int, fresh_importer._wrap_args({"v": 0}))
        self.assertIs(proxy_float, fresh_importer._wrap_args({"v": 0.0}))
        self.assertIs(proxy_bool, fresh_importer._wrap_args({"v": False}))
        self.assertIsNot(proxy_int, proxy_float)
        self.assertIsNot(proxy_int, proxy_bool)
        self.assertIsNot(proxy_float, proxy_bool)
        self.assertIs(type(proxy_int["v"]), int)
        self.assertIs(type(proxy_float["v"]), float)
        self.assertIs(type(proxy_bool["v"]), bool)

        # 2. Once `_single_arg_cache` reaches `_MAX_SINGLE_ARG_CACHE_SIZE`,
        # `_wrap_args` still returns a read-only `MappingProxyType` without
        # growing the cache past the limit.
        fresh_importer._single_arg_cache.clear()
        for i in range(trace_importing_fxt._MAX_SINGLE_ARG_CACHE_SIZE):
            fresh_importer._wrap_args({"k": i})
        self.assertEqual(
            len(fresh_importer._single_arg_cache),
            trace_importing_fxt._MAX_SINGLE_ARG_CACHE_SIZE,
        )

        overflow_proxy = fresh_importer._wrap_args(
            {"k": trace_importing_fxt._MAX_SINGLE_ARG_CACHE_SIZE}
        )
        self.assertIsInstance(overflow_proxy, types.MappingProxyType)
        self.assertEqual(
            overflow_proxy,
            {"k": trace_importing_fxt._MAX_SINGLE_ARG_CACHE_SIZE},
        )
        self.assertEqual(
            len(fresh_importer._single_arg_cache),
            trace_importing_fxt._MAX_SINGLE_ARG_CACHE_SIZE,
        )

    def test_get_counter_events_preserves_track_id(self) -> None:
        fresh_importer = trace_importing_fxt._FxtImporter(self.session)
        fresh_importer._args_map = {}
        mock_rows = [
            trace_importing_fxt._CounterRow(
                1000, 1000.0, "Rate:CPU:0", 0, 0, None, 10
            ),
            trace_importing_fxt._CounterRow(
                1000, 360.0, "Rate:CPU:3", 0, 0, None, 11
            ),
            trace_importing_fxt._CounterRow(
                1000, 250.0, "Demand:Constant:CPU:3", 0, 0, None, 12
            ),
            trace_importing_fxt._CounterRow(
                1000, 0.75, "cpu_load", 0, 0, None, 13
            ),
        ]
        with mock.patch.object(
            fresh_importer, "_query_as", return_value=iter(mock_rows)
        ):
            events = fresh_importer.get_counter_events().events

        expected: list[trace_model.Event] = [
            trace_model.CounterEvent(
                0,
                category="",
                name="Rate",
                start=trace_time.TimePoint(1000),
                pid=0,
                tid=0,
                args={"CPU": 1000.0},
            ),
            trace_model.CounterEvent(
                3,
                category="",
                name="Rate",
                start=trace_time.TimePoint(1000),
                pid=0,
                tid=0,
                args={"CPU": 360.0},
            ),
            trace_model.CounterEvent(
                3,
                category="",
                name="Demand:Constant",
                start=trace_time.TimePoint(1000),
                pid=0,
                tid=0,
                args={"CPU": 250.0},
            ),
            trace_model.CounterEvent(
                0,
                category="",
                name="cpu_load",
                start=trace_time.TimePoint(1000),
                pid=0,
                tid=0,
                args={"value": 0.75},
            ),
        ]
        test_utils.assertEventListsEqual(self, events, expected)

    def test_get_counter_events_keeps_colon_named_counters_separate(
        self,
    ) -> None:
        # Counters whose event names share a colon-separated prefix must stay
        # separate events, even when sampled at the same timestamp on the same CPU.
        fresh_importer = trace_importing_fxt._FxtImporter(self.session)
        fresh_importer._args_map = {}
        mock_rows = [
            trace_importing_fxt._CounterRow(
                1000, 250.0, "Demand:Constant:CPU:3", 0, 0, None, 10
            ),
            trace_importing_fxt._CounterRow(
                1000, 0.5, "Demand:Constant:Normalized:3", 0, 0, None, 11
            ),
            trace_importing_fxt._CounterRow(
                1000, 900.0, "Demand:Total:CPU:3", 0, 0, None, 12
            ),
        ]
        with mock.patch.object(
            fresh_importer, "_query_as", return_value=iter(mock_rows)
        ):
            events = fresh_importer.get_counter_events().events

        expected: list[trace_model.Event] = [
            trace_model.CounterEvent(
                3,
                category="",
                name="Demand:Constant",
                start=trace_time.TimePoint(1000),
                pid=0,
                tid=0,
                args={"CPU": 250.0, "Normalized": 0.5},
            ),
            trace_model.CounterEvent(
                3,
                category="",
                name="Demand:Total",
                start=trace_time.TimePoint(1000),
                pid=0,
                tid=0,
                args={"CPU": 900.0},
            ),
        ]
        test_utils.assertEventListsEqual(self, events, expected)

    def test_get_scheduling_records(self) -> None:
        scheduling = self.importer.get_scheduling_records()
        # Verify context switches scheduling tracks populated with actual trace records
        self.assertIn(0, scheduling.records)
        self.assertGreater(len(scheduling.records[0]), 0)
        first_switch = scheduling.records[0][0]
        self.assertIsInstance(first_switch, trace_model.ContextSwitch)

    def test_get_scheduling_records_queries_per_cpu(self) -> None:
        fresh_importer = trace_importing_fxt._FxtImporter(self.session)
        with mock.patch.object(
            fresh_importer, "_query_as", wraps=fresh_importer._query_as
        ) as mock_query_as:
            scheduling = fresh_importer.get_scheduling_records()

        first_call = mock_query_as.call_args_list[0]
        self.assertIs(first_call[0][0], trace_importing_fxt._CpuRow)

        cpus = list(scheduling.records.keys())
        sched_calls = [
            c
            for c in mock_query_as.call_args_list[1:]
            if c[0][0] is trace_importing_fxt._CpuSchedRow
        ]
        self.assertEqual(len(sched_calls), len(cpus))

    def _run_get_scheduling_records(
        self,
        rows_by_cpu: Mapping[int, Sequence[trace_importing_fxt._CpuSchedRow]],
    ) -> dict[int, list[trace_model.SchedulingRecord]]:
        cpus = sorted(rows_by_cpu.keys())

        def fake_query_as(row_type: Any, sql: str) -> Any:
            if row_type is trace_importing_fxt._CpuRow:
                return iter(trace_importing_fxt._CpuRow(cpu) for cpu in cpus)
            if row_type is trace_importing_fxt._CpuSchedRow:
                for cpu in cpus:
                    if (
                        f"COALESCE(s.cpu, 0) = {cpu}\n" in sql
                        and f"COALESCE(ts.cpu, ts.ucpu, 0) = {cpu}\n" in sql
                    ):
                        return iter(rows_by_cpu[cpu])
            raise AssertionError(f"Unexpected query ({row_type}): {sql}")

        with mock.patch.object(
            self.importer, "_query_as", side_effect=fake_query_as
        ):
            return self.importer.get_scheduling_records().records

    @staticmethod
    def _make_sched_row(
        ts: int,
        dur: int,
        end_state: str,
        tid: int,
        prio: int = 3122,
        state: str = "Running",
    ) -> trace_importing_fxt._CpuSchedRow:
        return trace_importing_fxt._CpuSchedRow(
            ts=ts,
            dur=dur,
            end_state=end_state,
            priority=prio,
            tid=tid,
            state=state,
        )

    @staticmethod
    def _make_switch(
        start_ns: int,
        in_tid: int,
        out_tid: int,
        in_prio: int = 3122,
        out_prio: int | None = 3122,
        out_state: trace_model.ThreadState = _RUNNING,
    ) -> trace_model.ContextSwitch:
        return trace_model.ContextSwitch(
            start=trace_time.TimePoint.from_epoch_delta(
                trace_time.TimeDelta.from_nanoseconds(start_ns)
            ),
            incoming_tid=in_tid,
            outgoing_tid=out_tid,
            incoming_prio=in_prio,
            outgoing_prio=out_prio,
            outgoing_state=out_state,
            args={},
        )

    @staticmethod
    def _make_waking(start_ns: int, tid: int) -> trace_model.Waking:
        return trace_model.Waking(
            start=trace_time.TimePoint.from_epoch_delta(
                trace_time.TimeDelta.from_nanoseconds(start_ns)
            ),
            tid=tid,
            prio=0,
            args={},
        )

    def test_get_scheduling_records_end_states(self) -> None:
        # Exercise every end_state code mapped by get_scheduling_records(),
        # plus an idle gap from 1500ns to 1600ns.
        records = self._run_get_scheduling_records(
            {
                0: [
                    self._make_sched_row(1000, 100, "R", 101),
                    self._make_sched_row(1100, 100, "S", 102),
                    self._make_sched_row(1200, 100, "D", 103),
                    self._make_sched_row(1300, 100, "T", 104),
                    self._make_sched_row(1400, 100, "Z", 105),
                    # Idle gap from 1500 to 1600.
                    self._make_sched_row(1600, 100, "X", 106),
                    self._make_sched_row(1700, 100, "R", 107),
                ]
            }
        )

        test_utils.assertSchedulingRecordsEqual(
            self,
            records,
            {
                0: [
                    self._make_switch(1000, 101, 0, out_prio=None),
                    self._make_switch(1100, 102, 101),
                    self._make_switch(1200, 103, 102, out_state=_BLOCKED),
                    self._make_switch(1300, 104, 103, out_state=_BLOCKED),
                    self._make_switch(1400, 105, 104, out_state=_SUSPENDED),
                    self._make_switch(
                        1500, 0, 105, in_prio=_IDLE, out_state=_DYING
                    ),
                    self._make_switch(1600, 106, 0, out_prio=_IDLE),
                    self._make_switch(1700, 107, 106, out_state=_DEAD),
                    self._make_switch(1800, 0, 107, in_prio=_IDLE),
                ]
            },
        )

    def test_get_scheduling_records_waking_during_idle_gap(self) -> None:
        # Thread 101 blocks at 1500ns, thread 102 wakes during the idle gap at
        # 2000ns, and thread 102 runs from 2500ns to 3000ns. The importer must
        # synthesize an idle switch at 1500ns before the waking event and a
        # trailing idle switch at 3000ns.
        records = self._run_get_scheduling_records(
            {
                0: [
                    self._make_sched_row(1000, 500, "S", 101),
                    self._make_sched_row(2000, 0, "", 102, prio=0, state="W"),
                    self._make_sched_row(2500, 500, "R", 102),
                ]
            }
        )

        test_utils.assertSchedulingRecordsEqual(
            self,
            records,
            {
                0: [
                    self._make_switch(1000, 101, 0, out_prio=None),
                    self._make_switch(
                        1500, 0, 101, in_prio=_IDLE, out_state=_BLOCKED
                    ),
                    self._make_waking(2000, tid=102),
                    self._make_switch(2500, 102, 0, out_prio=_IDLE),
                    self._make_switch(3000, 0, 102, in_prio=_IDLE),
                ]
            },
        )

    def test_get_scheduling_records_final_outgoing_switch(self) -> None:
        # CPU 0's final slice has a known duration (500ns) and must emit a
        # trailing switch to idle at 1500ns; CPU 1's final slice is open-ended
        # (dur=-1) and must not emit a trailing idle switch.
        records = self._run_get_scheduling_records(
            {
                0: [self._make_sched_row(1000, 500, "S", 101)],
                1: [self._make_sched_row(1000, -1, "", 201)],
            }
        )

        test_utils.assertSchedulingRecordsEqual(
            self,
            records,
            {
                0: [
                    self._make_switch(1000, 101, 0, out_prio=None),
                    self._make_switch(
                        1500, 0, 101, in_prio=_IDLE, out_state=_BLOCKED
                    ),
                ],
                1: [self._make_switch(1000, 201, 0, out_prio=None)],
            },
        )

    def test_get_scheduling_records_sparse_cpus(self) -> None:
        # Non-contiguous CPU IDs (0 and 4) must be preserved without populating
        # empty intermediate CPUs.
        records = self._run_get_scheduling_records(
            {
                0: [self._make_sched_row(1000, -1, "", 101)],
                4: [self._make_sched_row(2000, -1, "", 102)],
            }
        )

        test_utils.assertSchedulingRecordsEqual(
            self,
            records,
            {
                0: [self._make_switch(1000, 101, 0, out_prio=None)],
                4: [self._make_switch(2000, 102, 0, out_prio=None)],
            },
        )

    def test_get_scheduling_records_waking_only_cpu(self) -> None:
        # A CPU that only has thread_state waking events (no sched slices) is
        # still discovered and populated with Waking records at prio=0.
        records = self._run_get_scheduling_records(
            {2: [self._make_sched_row(5000, 0, "", 301, prio=_IDLE, state="W")]}
        )

        test_utils.assertSchedulingRecordsEqual(
            self,
            records,
            {2: [self._make_waking(5000, tid=301)]},
        )

    def test_get_flow_events(self) -> None:
        (
            _,
            slice_id_to_event,
            slice_parent_map,
        ) = self.importer.get_slices()
        with mock.patch.object(
            self.importer, "_query_as", wraps=self.importer._query_as
        ) as mock_query_as:
            flows = self.importer.get_flow_events(
                slice_id_to_event, slice_parent_map
            )

        flow_calls = [
            c
            for c in mock_query_as.call_args_list
            if c[0][0] is trace_importing_fxt._FlowRow
        ]
        self.assertEqual(len(flow_calls), 1)

        flow_events = [
            e for e in flows.events if isinstance(e, trace_model.FlowEvent)
        ]
        self.assertGreater(len(flow_events), 0)
        self.assertIsNotNone(flow_events[0].id)

    def test_get_flow_events_with_category_filtering(self) -> None:
        all_slices = self.importer.get_slices()
        target_cat = all_slices.events[0].category
        filtered_importer = trace_importing_fxt._FxtImporter(
            self.session, categories={target_cat}
        )
        filtered_slices = filtered_importer.get_slices()
        flows = filtered_importer.get_flow_events(
            filtered_slices.id_to_event, filtered_slices.parent_map
        )
        flow_events = [
            e for e in flows.events if isinstance(e, trace_model.FlowEvent)
        ]
        self.assertGreater(len(flow_events), 0)
        has_enclosing = any(
            f.enclosing_duration is not None for f in flow_events
        )
        self.assertTrue(has_enclosing)

    @staticmethod
    def _make_duration(
        name: str,
        start_us: float,
        duration_us: float = 100.0,
        category: str = "io",
        pid: int = 1,
        tid: int = 1,
        parent: trace_model.DurationEvent | None = None,
    ) -> trace_model.DurationEvent:
        return trace_model.DurationEvent(
            duration=trace_time.TimeDelta.from_microseconds(duration_us),
            parent=parent,
            category=category,
            name=name,
            start=trace_time.TimePoint.from_epoch_delta(
                trace_time.TimeDelta.from_microseconds(start_us)
            ),
            pid=pid,
            tid=tid,
            args={},
        )

    @staticmethod
    def _make_flow_chain(
        flow_id: str, durations: Sequence[trace_model.DurationEvent]
    ) -> list[trace_model.FlowEvent]:
        """Builds a linked START -> [STEP ...] -> END flow chain across `durations`."""
        flows: list[trace_model.FlowEvent] = []
        for idx, dur in enumerate(durations):
            if idx == 0:
                phase = trace_model.FlowEventPhase.START
            elif idx == len(durations) - 1:
                phase = trace_model.FlowEventPhase.END
            else:
                phase = trace_model.FlowEventPhase.STEP
            prev = flows[-1] if flows else None
            flow = trace_model.FlowEvent(
                id=flow_id,
                phase=phase,
                enclosing_duration=dur,
                previous_flow=prev,
                category=dur.category,
                name=dur.name,
                start=dur.start,
                pid=dur.pid,
                tid=dur.tid,
                args=dur.args,
            )
            if prev is not None:
                prev.next_flow = flow
            dur.child_flows = [*dur.child_flows, flow]
            flows.append(flow)
        return flows

    @staticmethod
    def _model_fxt_duration(
        name: str, pid: int = 7009
    ) -> trace_model.DurationEvent:
        """Returns a fresh detached copy of a DurationEvent from model.fxt's reference model."""
        for event in test_utils.get_test_model(is_fxt=True).all_events():
            if (
                isinstance(event, trace_model.DurationEvent)
                and event.name == name
                and event.pid == pid
            ):
                assert event.duration is not None
                return trace_model.DurationEvent(
                    duration=event.duration,
                    category=event.category,
                    name=event.name,
                    start=event.start,
                    pid=event.pid,
                    tid=event.tid,
                    args=event.args,
                )
        raise AssertionError(f"DurationEvent {name!r} (pid={pid}) not found")

    def test_get_flow_events_resolves_filtered_child_slices_to_kept_ancestors(
        self,
    ) -> None:
        # In model.fxt, flow 1 runs ReadSubBegin -> WriteSubBegin -> WriteSubEnd.
        # Filtering to only parent slices {"^Read$", "^Write$"} drops all three
        # flow endpoint slices; ancestor_slice resolution must map ReadSubBegin
        # to Read and WriteSubBegin/WriteSubEnd to Write, while suppressing the
        # intra-Write self-loop (Write -> Write).
        filtered_importer = trace_importing_fxt._FxtImporter(
            self.session, patterns={"^Read$", "^Write$"}
        )
        events = filtered_importer.get_events().events

        exp_read = self._model_fxt_duration("Read", pid=7009)
        exp_write = self._model_fxt_duration("Write", pid=7009)
        exp_read2 = self._model_fxt_duration("Read", pid=7010)
        exp_start, exp_end = self._make_flow_chain("1", [exp_read, exp_write])

        test_utils.assertEventListsEqual(
            self,
            events,
            [exp_read, exp_start, exp_write, exp_end, exp_read2],
        )

    def test_get_flow_events_contracts_unkept_intermediate_hops_in_fxt(
        self,
    ) -> None:
        # Keeping only "^Read$" and "^WriteSubEnd$" in model.fxt drops both
        # WriteSubBegin and its parent Write, so hop 1 (ReadSubBegin ->
        # WriteSubBegin) resolves resolved_out=Read, resolved_in=NULL, and hop 2
        # (WriteSubBegin -> WriteSubEnd) contracts across WriteSubBegin to
        # connect Read -> WriteSubEnd.
        filtered_importer = trace_importing_fxt._FxtImporter(
            self.session, patterns={"^Read$", "^WriteSubEnd$"}
        )
        events = filtered_importer.get_events().events

        exp_read = self._model_fxt_duration("Read", pid=7009)
        exp_write_sub_end = self._model_fxt_duration("WriteSubEnd", pid=7009)
        exp_read2 = self._model_fxt_duration("Read", pid=7010)
        exp_start, exp_end = self._make_flow_chain(
            "1", [exp_read, exp_write_sub_end]
        )

        test_utils.assertEventListsEqual(
            self,
            events,
            [exp_read, exp_start, exp_write_sub_end, exp_end, exp_read2],
        )

    def test_get_flow_events_contracts_multi_hop_chain_and_skips_cycles(
        self,
    ) -> None:
        # Multi-hop chain with multiple consecutive unkept hops, STEP upgrade,
        # cycle rejection, and duplicate-branch rejection:
        # Flow 99: 1 (kept) -> 2 (unkept) -> 3 (unkept) -> 4 (kept) -> 5 (unkept) -> 6 (kept),
        # plus a cycle attempt 6 -> 1 and a duplicate branch attempt 1 -> 7.
        dur_1 = self._make_duration("slice_1", start_us=1000.0)
        dur_4 = self._make_duration("slice_4", start_us=2000.0)
        dur_6 = self._make_duration("slice_6", start_us=3000.0)
        dur_7 = self._make_duration("slice_7", start_us=4000.0)
        slice_id_to_event: dict[int, trace_model.Event] = {
            1: dur_1,
            4: dur_4,
            6: dur_6,
            7: dur_7,
        }
        mock_flow_rows = [
            trace_importing_fxt._FlowRow(1, 2, 99, 1, None),
            trace_importing_fxt._FlowRow(2, 3, 99, None, None),
            trace_importing_fxt._FlowRow(3, 4, 99, None, 4),
            trace_importing_fxt._FlowRow(4, 5, 99, 4, None),
            trace_importing_fxt._FlowRow(5, 6, 99, None, 6),
            # Cycle back to already-visited slice 1 must be ignored.
            trace_importing_fxt._FlowRow(6, 1, 99, 6, 1),
            # Second outgoing branch from slice 1 to unvisited slice 7 must not
            # overwrite slice_1.next_flow.
            trace_importing_fxt._FlowRow(1, 7, 99, 1, 7),
        ]
        with mock.patch.object(
            self.importer, "_query_as", return_value=iter(mock_flow_rows)
        ):
            contracted = self.importer.get_flow_events(slice_id_to_event, {})

        exp_dur_1 = self._make_duration("slice_1", start_us=1000.0)
        exp_dur_4 = self._make_duration("slice_4", start_us=2000.0)
        exp_dur_6 = self._make_duration("slice_6", start_us=3000.0)
        exp_dur_7 = self._make_duration("slice_7", start_us=4000.0)
        exp_flows = self._make_flow_chain(
            "99", [exp_dur_1, exp_dur_4, exp_dur_6]
        )

        test_utils.assertEventListsEqual(
            self,
            [dur_1, dur_4, dur_6, dur_7, *contracted.events],
            [exp_dur_1, exp_dur_4, exp_dur_6, exp_dur_7, *exp_flows],
        )

    def test_get_slices_clears_parent_when_all_ancestors_filtered(
        self,
    ) -> None:
        # In model.fxt, when a parent slice (Write) is filtered out and only its
        # child (WriteSubBegin) is kept, parent_id resolves to NULL so the kept
        # child has no parent.
        importer_dropped_parent = trace_importing_fxt._FxtImporter(
            self.session, patterns={"^WriteSubBegin$"}
        )
        test_utils.assertEventListsEqual(
            self,
            importer_dropped_parent.get_events().events,
            [self._model_fxt_duration("WriteSubBegin", pid=7009)],
        )

    def test_get_slices_resolves_parent_id_to_nearest_kept_ancestor(
        self,
    ) -> None:
        # When an intermediate parent slice is filtered out and both a
        # grandparent and great-grandparent slice are kept, get_slices() SQL
        # resolves parent_id across the filtered parent to the nearest kept
        # ancestor (Grandparent, depth=1) rather than the root ancestor
        # (GreatGrandparent, depth=0), and _restore_slice_hierarchy links them.
        nested_trace = {
            "traceEvents": [
                {
                    "cat": "io",
                    "name": "GreatGrandparent",
                    "ph": "X",
                    "pid": 1,
                    "tid": 1,
                    "ts": 50,
                    "dur": 1200,
                },
                {
                    "cat": "io",
                    "name": "Grandparent",
                    "ph": "X",
                    "pid": 1,
                    "tid": 1,
                    "ts": 100,
                    "dur": 1000,
                },
                {
                    "cat": "io",
                    "name": "FilteredParent",
                    "ph": "X",
                    "pid": 1,
                    "tid": 1,
                    "ts": 200,
                    "dur": 600,
                },
                {
                    "cat": "io",
                    "name": "KeptChild",
                    "ph": "X",
                    "pid": 1,
                    "tid": 1,
                    "ts": 300,
                    "dur": 200,
                },
            ],
        }
        with tempfile.NamedTemporaryFile(mode="w", suffix=".json") as tmp:
            json.dump(nested_trace, tmp)
            tmp.flush()
            with PerfettoTraceProcessor(trace_path=tmp.name) as nested_session:
                importer_filtered = trace_importing_fxt._FxtImporter(
                    nested_session,
                    patterns={
                        "^GreatGrandparent$",
                        "^Grandparent$",
                        "^KeptChild$",
                    },
                )
                events = importer_filtered.get_events().events

        exp_great_grandparent = self._make_duration(
            "GreatGrandparent", start_us=50.0, duration_us=1200.0
        )
        exp_grandparent = self._make_duration(
            "Grandparent",
            start_us=100.0,
            duration_us=1000.0,
            parent=exp_great_grandparent,
        )
        exp_child = self._make_duration(
            "KeptChild",
            start_us=300.0,
            duration_us=200.0,
            parent=exp_grandparent,
        )
        exp_great_grandparent.child_durations = [exp_grandparent]
        exp_grandparent.child_durations = [exp_child]

        test_utils.assertEventListsEqual(
            self,
            events,
            [exp_great_grandparent, exp_grandparent, exp_child],
        )

    def test_tp_session_query_and_create_model(self) -> None:
        fxt_path = (
            pathlib.Path(__file__).resolve().parent.parent.parent
            / "runtime_deps"
            / "model.fxt"
        )
        with PerfettoTraceProcessor(trace_path=str(fxt_path)) as session:
            # Query directly against the trace session without building a model
            res = session.run_query("SELECT count(*) as cnt FROM slice")
            self.assertEqual(len(res), 1)
            self.assertGreater(res[0]["cnt"], 0)

            # Build model from existing session without closing the session
            model = trace_importing_fxt.create_model_from_tp_session(session)
            self.assertGreater(len(list(model.all_events())), 0)

            # Verify session is still active after building model
            res2 = session.run_query("SELECT count(*) as cnt FROM slice")
            self.assertEqual(res, res2)

        # After context manager exits, querying closed session raises RuntimeError
        with self.assertRaises(RuntimeError):
            session.run_query("SELECT count(*) FROM slice")

    def test_create_model_pauses_gc_and_restores_it(self) -> None:
        self.addCleanup(gc.enable if gc.isenabled() else gc.disable)
        for initially_enabled in (True, False):
            for fails in (False, True):
                with self.subTest(
                    initially_enabled=initially_enabled, fails=fails
                ):
                    gc_enabled_during_import: list[bool] = []

                    def spy(*args: Any) -> mock.Mock:
                        gc_enabled_during_import.append(gc.isenabled())
                        if fails:
                            raise RuntimeError("boom")
                        return mock.Mock()

                    if initially_enabled:
                        gc.enable()
                    else:
                        gc.disable()
                    with (
                        mock.patch.object(
                            trace_importing_fxt,
                            "construct_model",
                            side_effect=spy,
                        ),
                        (
                            self.assertRaises(RuntimeError)
                            if fails
                            else contextlib.nullcontext()
                        ),
                    ):
                        trace_importing_fxt.create_model_from_tp_session(
                            self.session
                        )
                    self.assertEqual(gc_enabled_during_import, [False])
                    self.assertEqual(gc.isenabled(), initially_enabled)


class ParseCounterTrackTest(unittest.TestCase):
    """Tests for splitting Perfetto counter track names."""

    def test_parse_counter_track(self) -> None:
        cases = (
            ("cpu_load", ("cpu_load", "value", None)),
            (
                "kmem_stats_a:zram_bytes",
                ("kmem_stats_a:zram_bytes", "value", None),
            ),
            ("Rate:CPU:3", ("Rate", "CPU", 3)),
            ("Demand:Constant:CPU:3", ("Demand:Constant", "CPU", 3)),
            # Names not ending in a numeric counter ID are kept whole.
            ("a:b:c", ("a:b:c", "value", None)),
            # So are names with a numeric suffix but fewer than three parts.
            ("cpu_load:0", ("cpu_load:0", "value", None)),
            (
                "cpu_usage:average_cpu_percentage:0",
                ("cpu_usage", "average_cpu_percentage", 0),
            ),
        )
        for track_name, expected in cases:
            with self.subTest(track_name=track_name):
                self.assertEqual(
                    trace_importing_fxt._parse_counter_track(track_name),
                    expected,
                )
