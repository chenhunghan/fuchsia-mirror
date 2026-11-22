# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for the app-launch diagnostic analysis plugin."""

import os
import tempfile
import unittest
from typing import Any
from unittest.mock import MagicMock, patch

from app_launch import (
    AppLaunchPlugin,
    _build_launch_cte,
    _build_spine_for_launch_row,
)
from plugins import PluginArgumentError
from trace_writing import fxt

# FXT scheduler constants (see fuchsia.dev/reference/tracing/format/records).
IDLE_WEIGHT = -0x80000000
THREAD_BLOCKED = 3


def _write_temp_trace(builder: fxt.Builder) -> str:
    fd, path = tempfile.mkstemp(suffix=".fxt")
    os.close(fd)
    builder.write_to_file(path)
    return path


class AppLaunchPluginTest(unittest.TestCase):
    """Tests for the app-launch analysis plugin using mock trace processor queries."""

    def setUp(self) -> None:
        self.all_tables = {
            "slice",
            "thread_track",
            "thread",
            "process",
            "thread_state",
        }

    def test_analyze_success_flow(self) -> None:
        """Tests that all 8 diagnostic sections are executed in order with expected outputs."""
        plugin = AppLaunchPlugin()
        mock_tp = MagicMock()
        mock_tp.get_tables.return_value = self.all_tables

        def run_query_mock(sql: str) -> list[dict[str, Any]]:
            if "pre_fork_ms" in sql:
                return [
                    {
                        "startup_id": 1,
                        "package": "com.android.performanceLaunch",
                        "process_name": "rformanceLaunch",
                        "startup_type": "cold",
                        "total_launch_ms": 420.5,
                        "pre_fork_ms": 18.2,
                        "process_fork_ms": 24.3,
                        "attach_app_ms": 31.0,
                        "bind_app_ms": 115.0,
                        "activity_lifecycle_ms": 82.0,
                        "first_frame_ui_ms": 65.0,
                        "first_frame_render_ms": 85.0,
                        "display_present_tail_ms": 12.4,
                    }
                ]
            elif "substages_with_sum" in sql:
                return [
                    {
                        "startup_id": 1,
                        "package": "com.android.performanceLaunch",
                        "total_launch_ms": 420.5,
                        "init": 135.2,
                        "draw": 85.0,
                        "render": 6.6,
                        "traversal": 58.4,
                        "animation": 0.0,
                        "scenic": 3.4,
                    }
                ]
            elif "open_dex_ms" in sql:
                return [
                    {
                        "startup_id": 1,
                        "package": "com.android.performanceLaunch",
                        "open_dex_ms": 48.5,
                        "dlopen_ms": 12.1,
                        "verify_class_ms": 14.0,
                        "class_init_ms": 9.3,
                        "get_resources_ms": 18.2,
                        "inflate_ms": 34.6,
                        "relayout_window_ms": 11.4,
                        "dequeue_buffer_ms": 22.0,
                        "shader_compile_ms": 19.5,
                        "queue_present_ms": 38.2,
                    }
                ]
            elif "running_cpu_ms" in sql:
                return [
                    {
                        "startup_id": 1,
                        "package": "com.android.performanceLaunch",
                        "segment": "S3_BindApplication",
                        "thread_name": "rformanceLaunch",
                        "segment_dur_ms": 115.0,
                        "running_cpu_ms": 70.0,
                        "runnable_starved_ms": 15.0,
                        "sleeping_blocked_ms": 30.0,
                        "running_pct": 60.9,
                    }
                ]
            elif "fxfs_page_in_ms" in sql:
                return [
                    {
                        "startup_id": 1,
                        "package": "com.android.performanceLaunch",
                        "main_binder_calls": 14,
                        "main_binder_ms": 26.4,
                        "monitor_contention_ms": 4.2,
                        "native_lock_contention_ms": 8.1,
                        "fxfs_page_in_ms": 19.8,
                        "sysmem_busy_ms": 16.5,
                        "selinux_hooks_ms": 1.3,
                    }
                ]
            elif "AS draw_frame_end_ts" in sql:
                return [
                    {
                        "startup_id": 1,
                        "package": "com.android.performanceLaunch",
                        "process_name": "rformanceLaunch",
                        "startup_type": "cold",
                        "t0_launch_start": 100_000_000,
                        "t1_start_proc_ts": 120_000_000,
                        "start_proc_dur": 20_000_000,
                        "t3b_activity_start_ts": 250_000_000,
                        "t5_draw_frame_start": 400_000_000,
                        "t6_launch_end": 500_000_000,
                        "main_utid": 10,
                        "render_utid": 11,
                        "start_req_utid": 5,
                        "start_req_end_ts": 115_000_000,
                        "start_proc_utid": 6,
                        "draw_frame_end_ts": 490_000_000,
                        "complete_utid": 7,
                    }
                ]
            elif "_pa_cp_ts_spans" in sql:
                return [
                    {
                        "span_id": 1,
                        "group_id": 1,
                        "seg_index": 2,
                        "utid": 10,
                        "tid": 1010,
                        "thread_name": "rformanceLaunch",
                        "pid": 1000,
                        "process_name": "rformanceLaunch",
                        "ts": 140_000_000,
                        "dur": 260_000_000,
                        "state": "Running",
                        "cpu": 2,
                        "waker_utid": 6,
                        "waker_thread_name": "ActivityManager",
                        "waker_process_name": "system_server",
                        "slice_id": 42,
                        "depth": 1,
                        "slice_name": "bindApplication",
                        "category": "starnix:atrace",
                        "slice_clip_ts": 140_000_000,
                        "slice_clip_dur": 260_000_000,
                    },
                    {
                        "span_id": 2,
                        "group_id": 1,
                        "seg_index": 3,
                        "utid": 11,
                        "tid": 1011,
                        "thread_name": "RenderThread",
                        "pid": 1000,
                        "process_name": "rformanceLaunch",
                        "ts": 400_000_000,
                        "dur": 100_000_000,
                        "state": "Running",
                        "cpu": 3,
                        "waker_utid": 10,
                        "waker_thread_name": "rformanceLaunch",
                        "waker_process_name": "rformanceLaunch",
                        "slice_id": 43,
                        "depth": 0,
                        "slice_name": "DrawFrames 101",
                        "category": "starnix:atrace",
                        "slice_clip_ts": 400_000_000,
                        "slice_clip_dur": 100_000_000,
                    },
                ]
            return []

        mock_tp.run_query.side_effect = run_query_mock

        with patch("app_launch.PerfettoTraceProcessor") as mock_tp_class:
            mock_tp_class.return_value.__enter__.return_value = mock_tp
            results = plugin.analyze(
                ["--package", "performanceLaunch", "--limit", "10"],
                "dummy_trace",
            )

        self.assertEqual(len(results), 8)

        # 1. Critical Path Phase Breakdown (Sequential Segments)
        self.assertEqual(
            results[0].name,
            "Critical Path Phase Breakdown (Sequential Segments)",
        )
        res0 = results[0].results
        assert res0 is not None
        self.assertEqual(len(res0), 1)
        self.assertEqual(res0[0]["pre_fork_ms"], 18.2)
        self.assertEqual(res0[0]["process_fork_ms"], 24.3)
        self.assertEqual(res0[0]["bind_app_ms"], 115.0)
        self.assertEqual(res0[0]["first_frame_render_ms"], 85.0)

        # 2. CUJ Sub-Stage Breakdown
        self.assertEqual(results[1].name, "CUJ Sub-Stage Breakdown")
        res1 = results[1].results
        assert res1 is not None
        self.assertEqual(len(res1), 1)
        self.assertEqual(res1[0]["init"], 135.2)
        self.assertEqual(res1[0]["traversal"], 58.4)
        self.assertEqual(res1[0]["draw"], 85.0)
        self.assertEqual(res1[0]["scenic"], 3.4)

        # 3. Intra-Segment Sub-Operation Breakdown
        self.assertEqual(
            results[2].name, "Intra-Segment Sub-Operation Breakdown"
        )
        res2 = results[2].results
        assert res2 is not None
        self.assertEqual(len(res2), 1)
        self.assertEqual(res2[0]["open_dex_ms"], 48.5)
        self.assertEqual(res2[0]["inflate_ms"], 34.6)

        # 4. Critical Path Thread State Breakdown
        self.assertEqual(
            results[3].name, "Critical Path Thread State Breakdown"
        )
        res3 = results[3].results
        assert res3 is not None
        self.assertEqual(len(res3), 1)
        self.assertEqual(res3[0]["segment"], "S3_BindApplication")
        self.assertEqual(res3[0]["running_cpu_ms"], 70.0)

        # 5. Cross-Layer Contention & Fuchsia Subsystem Attribution
        self.assertEqual(
            results[4].name,
            "Cross-Layer Contention & Fuchsia Subsystem Attribution",
        )
        res4 = results[4].results
        assert res4 is not None
        self.assertEqual(len(res4), 1)
        self.assertEqual(res4[0]["fxfs_page_in_ms"], 19.8)
        self.assertEqual(res4[0]["sysmem_busy_ms"], 16.5)

        # 6. Critical Path Process & Thread Breakdown
        self.assertEqual(
            results[5].name, "Critical Path Process & Thread Breakdown"
        )
        res5 = results[5].results
        assert res5 is not None
        self.assertEqual(len(res5), 2)
        self.assertEqual(res5[0]["startup_id"], 1)
        self.assertEqual(res5[0]["startup_type"], "cold")
        self.assertEqual(res5[0]["thread_name"], "rformanceLaunch")
        self.assertEqual(res5[0]["critical_path_ms"], 260.0)
        self.assertEqual(res5[1]["thread_name"], "RenderThread")
        self.assertEqual(res5[1]["critical_path_ms"], 100.0)

        # 7. Critical Path Top Blocking Slices
        self.assertEqual(results[6].name, "Critical Path Top Blocking Slices")
        res6 = results[6].results
        assert res6 is not None
        self.assertEqual(len(res6), 2)
        self.assertEqual(res6[0]["slice_name"], "bindApplication")
        self.assertEqual(res6[0]["critical_path_ms"], 260.0)

        # 8. Critical Path Chronological Span Timeline
        self.assertEqual(
            results[7].name, "Critical Path Chronological Span Timeline"
        )
        res7 = results[7].results
        assert res7 is not None
        self.assertEqual(len(res7), 2)
        self.assertEqual(res7[0]["slice_name"], "bindApplication")
        self.assertEqual(res7[1]["slice_name"], "DrawFrames 101")
        self.assertIn("wakeup", res7[1]["handoff_from"])

    def test_analyze_empty_results(self) -> None:
        """Tests that empty query results format as empty lists across all 8 sections."""
        plugin = AppLaunchPlugin()
        mock_tp = MagicMock()
        mock_tp.get_tables.return_value = self.all_tables
        mock_tp.run_query.return_value = []

        with patch("app_launch.PerfettoTraceProcessor") as mock_tp_class:
            mock_tp_class.return_value.__enter__.return_value = mock_tp
            results = plugin.analyze([], "dummy_trace")

        self.assertEqual(len(results), 8)
        for section in results:
            self.assertTrue(section.name)
            self.assertEqual(section.results, [])

    def test_analyze_missing_schema(self) -> None:
        """Tests that missing schema tables are handled gracefully with section error messages."""
        plugin = AppLaunchPlugin()
        mock_tp = MagicMock()
        mock_tp.get_tables.return_value = set()
        mock_tp.run_query.return_value = []

        with patch("app_launch.PerfettoTraceProcessor") as mock_tp_class:
            mock_tp_class.return_value.__enter__.return_value = mock_tp
            results = plugin.analyze([], "dummy_trace")

        self.assertEqual(len(results), 8)
        for section in results:
            assert section.error is not None
            self.assertIn("Required schema tables/views missing", section.error)

    def test_analyze_query_failure(self) -> None:
        """Tests that runtime SQL query failures return formatted error dicts without crashing."""
        plugin = AppLaunchPlugin()
        mock_tp = MagicMock()
        mock_tp.get_tables.return_value = self.all_tables
        mock_tp.run_query.side_effect = RuntimeError(
            "database disk image is malformed"
        )

        with patch("app_launch.PerfettoTraceProcessor") as mock_tp_class:
            mock_tp_class.return_value.__enter__.return_value = mock_tp
            results = plugin.analyze([], "dummy_trace")

        self.assertEqual(len(results), 8)
        for section in results:
            assert section.error is not None
            self.assertIn(
                "Query execution failed: database disk image is malformed",
                section.error,
            )

    def test_analyze_invalid_limit(self) -> None:
        """Tests that invalid, non-positive, or non-integer limits raise PluginArgumentError."""
        plugin = AppLaunchPlugin()
        for invalid_limit in ["0", "-5"]:
            with self.subTest(limit=invalid_limit):
                with self.assertRaisesRegex(
                    PluginArgumentError, "Limit must be a positive integer"
                ):
                    plugin.analyze(["--limit", invalid_limit], "dummy_trace")

        with self.assertRaisesRegex(PluginArgumentError, "invalid int value"):
            plugin.analyze(["--limit", "abc"], "dummy_trace")

    def test_analyze_scaling_option(self) -> None:
        """Tests that --scaling proportional injects proportional SQL expressions and validates choices."""
        plugin = AppLaunchPlugin()
        mock_tp = MagicMock()
        mock_tp.get_tables.return_value = self.all_tables
        captured_queries: list[str] = []

        def run_query_mock(sql: str) -> list[dict[str, Any]]:
            captured_queries.append(sql)
            return []

        mock_tp.run_query.side_effect = run_query_mock

        with patch("app_launch.PerfettoTraceProcessor") as mock_tp_class:
            mock_tp_class.return_value.__enter__.return_value = mock_tp
            results = plugin.analyze(
                ["--scaling", "proportional"], "dummy_trace"
            )

        self.assertEqual(len(results), 8)
        self.assertIn("stage_sum > 0 THEN", captured_queries[1])

        with patch("app_launch.PerfettoTraceProcessor") as mock_tp_class:
            mock_tp_class.return_value.__enter__.return_value = mock_tp
            plugin.analyze(["--scaling", "raw"], "dummy_trace")

        # Scaling applies only to the CUJ sub-stages: the critical path segments already
        # sum to the launch duration. Each run executes 6 queries when spine rows are empty.
        self.assertEqual(captured_queries[0], captured_queries[6])
        self.assertNotIn("stage_sum > 0 THEN", captured_queries[7])

        with self.assertRaisesRegex(PluginArgumentError, "invalid choice"):
            plugin.analyze(["--scaling", "invalid"], "dummy_trace")

    def test_critical_path_segments_cover_warm_launches(self) -> None:
        """Tests that warm launches get a pre-activity segment and gaps are reported."""
        plugin = AppLaunchPlugin()
        mock_tp = MagicMock()
        mock_tp.get_tables.return_value = self.all_tables
        captured_queries: list[str] = []

        def run_query_mock(sql: str) -> list[dict[str, Any]]:
            captured_queries.append(sql)
            return []

        mock_tp.run_query.side_effect = run_query_mock

        with patch("app_launch.PerfettoTraceProcessor") as mock_tp_class:
            mock_tp_class.return_value.__enter__.return_value = mock_tp
            plugin.analyze([], "dummy_trace")

        query = captured_queries[0]
        # Without bindApplication, the launch start to the first activityStart or
        # activityResume is its own segment instead of being left out.
        self.assertIn(
            "THEN t3b_activity_start_ts - t0b_activity_start_request_ts",
            query,
        )
        self.assertIn("AS pre_activity_ms", query)
        self.assertIn("COALESCE(pre_activity_dur, 0)", query)
        # Time no segment covers is reported, so the columns sum to total_launch_ms.
        self.assertIn(
            "ROUND((total_launch_dur - seg_sum) / 1e6, 2) AS unattributed_ms",
            query,
        )

    def _capture_queries(self, args: list[str]) -> list[str]:
        plugin = AppLaunchPlugin()
        mock_tp = MagicMock()
        mock_tp.get_tables.return_value = self.all_tables
        captured_queries: list[str] = []

        def run_query_mock(sql: str) -> list[dict[str, Any]]:
            captured_queries.append(sql)
            return []

        mock_tp.run_query.side_effect = run_query_mock
        with patch("app_launch.PerfettoTraceProcessor") as mock_tp_class:
            mock_tp_class.return_value.__enter__.return_value = mock_tp
            plugin.analyze(args, "dummy_trace")
        return captured_queries

    def test_critical_path_reports_time_from_activity_start(self) -> None:
        """Tests that launches are also measured from ActivityTaskManager's START."""
        query = self._capture_queries([])[0]
        # START is the last resolveIntent before shouldAbortBackgroundActivityStart.
        self.assertIn("sab.name = 'shouldAbortBackgroundActivityStart'", query)
        self.assertIn("SELECT MAX(ri.ts)", query)
        self.assertIn(
            "(t6_launch_end - t0b_activity_start_request_ts) AS displayed_dur",
            query,
        )
        self.assertIn(
            "(t0b_activity_start_request_ts - t0_launch_start) AS pre_start_dur",
            query,
        )
        # pre_start is its own segment, so the segments still sum to the launch.
        self.assertIn("COALESCE(pre_start_dur, 0)", query)
        self.assertIn(
            "(t1_start_proc_ts - t0b_activity_start_request_ts) AS pre_fork_dur",
            query,
        )
        # Without a START marker, the launch start stands in for it, so displayed_ms is
        # always a number.
        self.assertRegex(
            query,
            r"LIMIT 1\s*\),\s*m\.t0_launch_start\s*\) AS t0b_activity_start_request_ts",
        )

    def test_hot_launches_resolve_process_and_first_frame(self) -> None:
        """Tests that hot launches find a long truncated process name and the right frame."""
        query = self._capture_queries([])[0]
        # Starnix can truncate long process names to a prefix longer than 15 characters,
        # e.g. 'com.google.android.apps.nexusla'.
        self.assertEqual(
            query.count(
                "(LENGTH(p.name) >= 15 AND r.package GLOB p.name || '*')"
            ),
            2,
        )
        # Frames drawn before the app starts handling the launch (e.g. a hot launcher's
        # animations) are not the launch's first frame.
        self.assertIn(
            "a.name IN ('bindApplication', 'activityStart', 'activityResume')",
            query,
        )

    def test_contention_columns_cover_only_ui_threads(self) -> None:
        """Tests that binder and lock columns count outermost slices on the app's threads."""
        query = self._capture_queries([])[4]
        # Starnix emits BinderIoctl rather than the Linux binder transaction events.
        self.assertIn("s.name = 'BinderIoctl'", query)
        self.assertIn("AS main_binder_calls", query)
        self.assertIn("tt.utid IN (l.main_utid)\n", query)
        self.assertIn("tt.utid IN (l.main_utid, l.render_utid)\n", query)
        self.assertNotIn("t.upid = l.upid OR p.name = 'system_server'", query)
        # Nested matches are skipped and slices are clipped to the launch.
        self.assertIn("NOT EXISTS (", query)
        self.assertIn("FROM ancestor_slice(s.id) anc", query)
        self.assertIn(
            "SUM(MIN(l.t6_launch_end, s.ts + s.dur) - MAX(l.t0_launch_start, s.ts))",
            query,
        )
        # sysmem is reported as its own busy time, not summed across processes.
        self.assertIn("AS sysmem_busy_ms", query)
        self.assertNotIn("BufferCollection", query)

    def test_cuj_substages_count_self_time_of_nested_slices(self) -> None:
        """Tests that nested sub-stage slices are not counted twice."""
        plugin = AppLaunchPlugin()
        mock_tp = MagicMock()
        mock_tp.get_tables.return_value = self.all_tables
        captured_queries: list[str] = []

        def run_query_mock(sql: str) -> list[dict[str, Any]]:
            captured_queries.append(sql)
            return []

        mock_tp.run_query.side_effect = run_query_mock

        with patch("app_launch.PerfettoTraceProcessor") as mock_tp_class:
            mock_tp_class.return_value.__enter__.return_value = mock_tp
            plugin.analyze([], "dummy_trace")

        query = captured_queries[1]
        # Each sub-stage slice finds its nearest sub-stage ancestor, which subtracts it.
        self.assertIn("FROM ancestor_slice(a.slice_id) anc", query)
        self.assertIn("c.stage_parent_id = p.slice_id", query)
        for stage in ["init", "traversal", "render", "draw", "animation"]:
            with self.subTest(stage=stage):
                self.assertIn(
                    f"SUM(CASE WHEN a.stage = '{stage}' THEN a.self_dur END)",
                    query,
                )
        # Slices are clipped to the launch end.
        self.assertIn(
            "MIN(s.ts + s.dur, m.t6_launch_end) - s.ts AS clipped_dur", query
        )

    def test_build_launch_cte_escapes_package_filter(self) -> None:
        """Tests that package filter quotes are escaped in SQL CTE construction."""
        cte = _build_launch_cte("foo'bar")
        self.assertIn("*foo''bar*", cte)

    def test_analyze_multiple_launches_in_single_trace(self) -> None:
        """Tests that multiple launches (e.g. cold + warm or multiple apps) in one trace are preserved."""
        plugin = AppLaunchPlugin()
        mock_tp = MagicMock()
        mock_tp.get_tables.return_value = self.all_tables
        captured_queries: list[str] = []

        def run_query_mock(sql: str) -> list[dict[str, Any]]:
            captured_queries.append(sql)
            if "pre_fork_ms" in sql:
                return [
                    {
                        "startup_id": 14,
                        "package": "com.google.android.calendar",
                        "process_name": "com.google.android.calendar",
                        "startup_type": "cold",
                        "total_launch_ms": 1571.6,
                    },
                    {
                        "startup_id": 21,
                        "package": "com.google.android.calendar",
                        "process_name": "com.google.android.calendar",
                        "startup_type": "warm",
                        "total_launch_ms": 209.95,
                    },
                ]
            elif "substages_with_sum" in sql:
                return [
                    {
                        "startup_id": 14,
                        "package": "com.google.android.calendar",
                        "total_launch_ms": 1571.6,
                        "init": 389.49,
                        "draw": 57.53,
                        "render": 6.66,
                        "traversal": 86.26,
                        "animation": 0.0,
                        "scenic": 2.38,
                    },
                    {
                        "startup_id": 21,
                        "package": "com.google.android.calendar",
                        "total_launch_ms": 209.95,
                        "init": 73.45,
                        "draw": 51.27,
                        "render": 2.55,
                        "traversal": 6.53,
                        "animation": 0.0,
                        "scenic": 0.77,
                    },
                ]
            elif "AS draw_frame_end_ts" in sql:
                return [
                    {
                        "startup_id": 14,
                        "package": "com.google.android.calendar",
                        "process_name": "com.google.android.calendar",
                        "startup_type": "cold",
                        "t0_launch_start": 100_000_000,
                        "t1_start_proc_ts": 120_000_000,
                        "start_proc_dur": 20_000_000,
                        "t3b_activity_start_ts": 250_000_000,
                        "t5_draw_frame_start": 400_000_000,
                        "t6_launch_end": 500_000_000,
                        "main_utid": 10,
                        "render_utid": 11,
                        "start_req_utid": 5,
                        "start_req_end_ts": 115_000_000,
                        "start_proc_utid": 6,
                        "draw_frame_end_ts": 490_000_000,
                        "complete_utid": 7,
                    },
                    {
                        "startup_id": 21,
                        "package": "com.google.android.calendar",
                        "process_name": "com.google.android.calendar",
                        "startup_type": "warm",
                        "t0_launch_start": 800_000_000,
                        "t1_start_proc_ts": None,
                        "start_proc_dur": None,
                        "t3b_activity_start_ts": 830_000_000,
                        "t5_draw_frame_start": 920_000_000,
                        "t6_launch_end": 1_000_000_000,
                        "main_utid": 10,
                        "render_utid": 11,
                        "start_req_utid": 5,
                        "start_req_end_ts": 825_000_000,
                        "start_proc_utid": None,
                        "draw_frame_end_ts": 990_000_000,
                        "complete_utid": 7,
                    },
                ]
            elif "_pa_cp_ts_spans" in sql:
                return [
                    {
                        "span_id": 1,
                        "group_id": 1,
                        "seg_index": 2,
                        "utid": 10,
                        "tid": 1010,
                        "thread_name": "calendar_main",
                        "pid": 1000,
                        "process_name": "com.google.android.calendar",
                        "ts": 140_000_000,
                        "dur": 360_000_000,
                        "state": "Running",
                        "cpu": 0,
                        "waker_utid": 6,
                        "waker_thread_name": "ActivityManager",
                        "waker_process_name": "system_server",
                        "slice_id": 101,
                        "depth": 0,
                        "slice_name": "bindApplication",
                        "category": "atrace",
                        "slice_clip_ts": 140_000_000,
                        "slice_clip_dur": 360_000_000,
                    },
                    {
                        "span_id": 2,
                        "group_id": 2,
                        "seg_index": 1,
                        "utid": 11,
                        "tid": 1011,
                        "thread_name": "RenderThread",
                        "pid": 1000,
                        "process_name": "com.google.android.calendar",
                        "ts": 825_000_000,
                        "dur": 175_000_000,
                        "state": "Running",
                        "cpu": 1,
                        "waker_utid": 10,
                        "waker_thread_name": "calendar_main",
                        "waker_process_name": "com.google.android.calendar",
                        "slice_id": 102,
                        "depth": 0,
                        "slice_name": "DrawFrames 2",
                        "category": "atrace",
                        "slice_clip_ts": 825_000_000,
                        "slice_clip_dur": 175_000_000,
                    },
                ]
            return []

        mock_tp.run_query.side_effect = run_query_mock

        with patch("app_launch.PerfettoTraceProcessor") as mock_tp_class:
            mock_tp_class.return_value.__enter__.return_value = mock_tp
            results = plugin.analyze([], "dummy_multi_launch_trace")

        res0 = results[0].results

        assert res0 is not None

        self.assertEqual(len(res0), 2)
        self.assertEqual(res0[0]["startup_type"], "cold")
        self.assertEqual(res0[1]["startup_type"], "warm")
        res1 = results[1].results
        assert res1 is not None
        self.assertEqual(len(res1), 2)
        self.assertIn(
            "USING (startup_id, t0_launch_start)", captured_queries[1]
        )
        self.assertIn(
            "USING (startup_id, t0_launch_start)", captured_queries[2]
        )
        res5 = results[5].results
        res6 = results[6].results
        res7 = results[7].results
        assert res5 is not None and res6 is not None and res7 is not None
        self.assertEqual(
            [(r["startup_id"], r["startup_type"]) for r in res5],
            [(14, "cold"), (21, "warm")],
            f"multi-launch thread breakdown={res5}",
        )
        self.assertEqual(
            [r["startup_id"] for r in res6],
            [14, 21],
            f"multi-launch top slices={res6}",
        )
        self.assertEqual(
            [r["startup_id"] for r in res7],
            [14, 21],
            f"multi-launch timeline={res7}",
        )

    def test_build_spine_for_cold_launch_row(self) -> None:
        """Tests that cold launch spine segments tile [t0_launch_start, t6_launch_end] cleanly."""
        row: dict[str, Any] = {
            "t0_launch_start": 100_000_000,
            "t1_start_proc_ts": 120_000_000,
            "start_proc_dur": 25_000_000,
            "t3b_activity_start_ts": 210_000_000,
            "t5_draw_frame_start": 380_000_000,
            "t6_launch_end": 500_000_000,
            "main_utid": 10,
            "render_utid": 11,
            "start_req_utid": 5,
            "start_req_end_ts": 115_000_000,
            "start_proc_utid": 6,
            "draw_frame_end_ts": 485_000_000,
            "complete_utid": 7,
        }
        segs = _build_spine_for_launch_row(row, group_id=3)
        self.assertEqual(
            [(s.utid, s.ts, s.dur, s.handoff_reason) for s in segs],
            [
                (5, 100_000_000, 15_000_000, "launch_intent"),
                (6, 115_000_000, 30_000_000, "start_proc"),
                (10, 145_000_000, 235_000_000, "app_main"),
                (11, 380_000_000, 105_000_000, "render_thread"),
                (7, 485_000_000, 15_000_000, "launch_complete"),
            ],
            f"_build_spine_for_launch_row({row!r}) produced unexpected segments: {segs!r}",
        )
        self.assertEqual(
            sum(s.dur for s in segs),
            row["t6_launch_end"] - row["t0_launch_start"],
            f"_build_spine_for_launch_row({row!r}) did not conserve duration",
        )

    def test_build_spine_preserves_zero_utid(self) -> None:
        """Tests that `utid == 0` is preserved and never skipped as falsy."""
        row: dict[str, Any] = {
            "t0_launch_start": 100_000_000,
            "t1_start_proc_ts": 120_000_000,
            "start_proc_dur": 10_000_000,
            "t3b_activity_start_ts": None,
            "t5_draw_frame_start": None,
            "t6_launch_end": 200_000_000,
            "main_utid": 0,
            "render_utid": None,
            "start_req_utid": None,
            "start_req_end_ts": None,
            "start_proc_utid": None,
            "draw_frame_end_ts": None,
            "complete_utid": None,
        }
        segs = _build_spine_for_launch_row(row, group_id=1)
        self.assertEqual(
            [(s.utid, s.ts, s.dur) for s in segs],
            [(0, 100_000_000, 100_000_000)],
            f"_build_spine_for_launch_row({row!r}) = {segs!r}",
        )

    def test_build_spine_for_warm_launch_row_merges_same_utid(self) -> None:
        """Tests warm launch spine tiling and merging of adjacent same-utid segments."""
        row: dict[str, Any] = {
            "t0_launch_start": 200_000_000,
            "t1_start_proc_ts": None,
            "start_proc_dur": None,
            "t3b_activity_start_ts": 240_000_000,
            "t5_draw_frame_start": 350_000_000,
            "t6_launch_end": 420_000_000,
            "main_utid": 20,
            "render_utid": 21,
            "start_req_utid": 8,
            "start_req_end_ts": 250_000_000,
            "start_proc_utid": None,
            "draw_frame_end_ts": 420_000_000,
            "complete_utid": 21,
        }
        segs = _build_spine_for_launch_row(row, group_id=1)
        self.assertEqual(
            [(s.utid, s.ts, s.dur, s.handoff_reason) for s in segs],
            [
                (8, 200_000_000, 40_000_000, "launch_intent"),
                (20, 240_000_000, 110_000_000, "app_main"),
                (21, 350_000_000, 70_000_000, "render_thread"),
            ],
            f"_build_spine_for_launch_row({row!r}) produced unexpected segments: {segs!r}",
        )
        self.assertEqual(
            sum(s.dur for s in segs),
            row["t6_launch_end"] - row["t0_launch_start"],
            f"_build_spine_for_launch_row({row!r}) did not conserve duration",
        )

    def test_build_spine_degenerate_inputs_return_empty(self) -> None:
        """Tests that missing/inverted timestamps or all-None utids return []."""
        for bad_row in (
            {},
            {"t0_launch_start": 100, "t6_launch_end": 50, "main_utid": 1},
            {"t0_launch_start": 100, "t6_launch_end": 200},
        ):
            self.assertEqual(
                _build_spine_for_launch_row(bad_row, group_id=1),
                [],
                msg=f"_build_spine_for_launch_row({bad_row!r}) should be []",
            )

    def test_build_spine_post_fork_and_zero_timestamp_handoff(self) -> None:
        """Tests `t1b_post_fork_ts` early child handoff and `0` milestone timestamp."""
        row: dict[str, Any] = {
            "t0_launch_start": 0,
            "t1_start_proc_ts": 0,
            "start_proc_dur": 30_000_000,
            "t1b_post_fork_ts": 18_000_000,
            "t5_draw_frame_start": 60_000_000,
            "t6_launch_end": 100_000_000,
            "main_utid": 10,
            "render_utid": 11,
            "start_proc_utid": 6,
        }
        segs = _build_spine_for_launch_row(row, group_id=2)
        self.assertEqual(
            [(s.utid, s.ts, s.dur, s.handoff_reason) for s in segs],
            [
                (6, 0, 18_000_000, "start_proc"),
                (10, 18_000_000, 42_000_000, "app_main"),
                (11, 60_000_000, 40_000_000, "render_thread"),
            ],
            f"_build_spine_for_launch_row({row!r}) = {segs!r}",
        )


class AppLaunchEndToEndTest(unittest.TestCase):
    """End-to-end test exercising `AppLaunchPlugin` against a real PerfettoTraceProcessor FXT trace."""

    @staticmethod
    def _remove_file_if_exists(path: str) -> None:
        if os.path.exists(path):
            os.remove(path)

    def test_end_to_end_cold_launch_with_real_trace_processor(self) -> None:
        """Verifies all 8 `app-launch` sections execute and conserve critical-path time on real SQL."""
        ms = 1_000_000
        b = fxt.Builder()
        b.add_process(200, "system_server")
        b.add_thread(200, 201, "Binder:200_1")
        b.add_thread(200, 202, "ActivityManager:procStart")
        b.add_thread(200, 203, "wm_complete")

        b.add_process(300, "com.example.app")
        b.add_thread(300, 301, "com.example.app")
        b.add_thread(300, 302, "RenderThread")
        idle_tid = 9999

        # SDK 33+ async launch slice [100..400 ms] on system_server.
        b.add_async_begin(
            "wm", "launchingActivity#1", 100 * ms, 200, 201, async_id=1
        )
        b.add_async_end(
            "wm", "launchingActivity#1", 400 * ms, 200, 203, async_id=1
        )

        # 1. Binder:200_1 (201) runs [100..115 ms] with shouldAbortBackgroundActivityStart [102..115 ms].
        b.add_thread_wakeup(99 * ms, 0, 201)
        b.add_context_switch(
            100 * ms,
            0,
            THREAD_BLOCKED,
            idle_tid,
            201,
            outgoing_priority=IDLE_WEIGHT,
        )
        b.add_duration_begin(
            "wm", "shouldAbortBackgroundActivityStart", 102 * ms, 200, 201
        )
        b.add_duration_end(
            "wm", "shouldAbortBackgroundActivityStart", 115 * ms, 200, 201
        )
        b.add_thread_wakeup(115 * ms, 1, 202, waker_tid=201)
        b.add_context_switch(
            115 * ms,
            0,
            THREAD_BLOCKED,
            201,
            idle_tid,
            incoming_priority=IDLE_WEIGHT,
        )

        # 2. ActivityManager:procStart (202) runs [115..145 ms] with Start proc:com.example.app [120..145 ms].
        b.add_context_switch(
            115 * ms,
            1,
            THREAD_BLOCKED,
            idle_tid,
            202,
            outgoing_priority=IDLE_WEIGHT,
        )
        b.add_duration_begin(
            "am", "Start proc:com.example.app", 120 * ms, 200, 202
        )
        b.add_duration_end(
            "am", "Start proc:com.example.app", 145 * ms, 200, 202
        )
        b.add_thread_wakeup(145 * ms, 0, 301, waker_tid=202)
        b.add_context_switch(
            145 * ms,
            1,
            THREAD_BLOCKED,
            202,
            idle_tid,
            incoming_priority=IDLE_WEIGHT,
        )

        # 3. App main thread (301) runs [145..320 ms]: bindApplication, activityStart, Choreographer#doFrame.
        b.add_context_switch(
            145 * ms,
            0,
            THREAD_BLOCKED,
            idle_tid,
            301,
            outgoing_priority=IDLE_WEIGHT,
        )
        b.add_duration_begin("am", "bindApplication", 160 * ms, 300, 301)
        b.add_duration_end("am", "bindApplication", 220 * ms, 300, 301)
        b.add_duration_begin("wm", "activityStart", 225 * ms, 300, 301)
        b.add_duration_end("wm", "activityStart", 260 * ms, 300, 301)
        b.add_duration_begin(
            "view", "Choreographer#doFrame 1", 280 * ms, 300, 301
        )
        b.add_thread_wakeup(320 * ms, 1, 302, waker_tid=301)
        b.add_duration_end(
            "view", "Choreographer#doFrame 1", 320 * ms, 300, 301
        )
        b.add_context_switch(
            320 * ms,
            0,
            THREAD_BLOCKED,
            301,
            idle_tid,
            incoming_priority=IDLE_WEIGHT,
        )

        # 4. RenderThread (302) runs [320..390 ms] with TWO DrawFrames slices ([320..360] and [365..390]).
        b.add_context_switch(
            320 * ms,
            1,
            THREAD_BLOCKED,
            idle_tid,
            302,
            outgoing_priority=IDLE_WEIGHT,
        )
        b.add_duration_begin("hwui", "DrawFrames 1", 320 * ms, 300, 302)
        b.add_duration_end("hwui", "DrawFrames 1", 360 * ms, 300, 302)
        b.add_duration_begin("hwui", "DrawFrames 2", 365 * ms, 300, 302)
        b.add_duration_end("hwui", "DrawFrames 2", 390 * ms, 300, 302)
        b.add_thread_wakeup(390 * ms, 2, 203, waker_tid=302)
        b.add_context_switch(
            390 * ms,
            1,
            THREAD_BLOCKED,
            302,
            idle_tid,
            incoming_priority=IDLE_WEIGHT,
        )

        # 5. wm_complete (203) runs [390..400 ms] and emits launchingActivity#1:completed-cold:com.example.app.
        b.add_context_switch(
            390 * ms,
            2,
            THREAD_BLOCKED,
            idle_tid,
            203,
            outgoing_priority=IDLE_WEIGHT,
        )
        b.add_instant(
            "wm",
            "launchingActivity#1:completed-cold:com.example.app",
            400 * ms,
            200,
            203,
        )
        b.add_context_switch(
            405 * ms,
            2,
            THREAD_BLOCKED,
            203,
            idle_tid,
            incoming_priority=IDLE_WEIGHT,
        )

        trace_path = _write_temp_trace(b)
        self.addCleanup(self._remove_file_if_exists, trace_path)

        args = ["--package", "com.example.app"]
        sections = AppLaunchPlugin().analyze(args, trace_path)
        self.assertEqual(
            len(sections),
            8,
            f"AppLaunchPlugin().analyze({args}) returned {len(sections)} sections",
        )
        for sec in sections:
            self.assertIsNone(
                sec.error,
                f"section {sec.name!r} errored for {args}: {sec.error}",
            )

        seg_rows = sections[0].results
        cp_thread_rows = sections[5].results
        assert seg_rows is not None and cp_thread_rows is not None
        self.assertEqual(
            len(seg_rows),
            1,
            f"expected 1 launch row for {args}, got {seg_rows}",
        )
        self.assertAlmostEqual(
            seg_rows[0]["total_launch_ms"],
            300.0,
            places=2,
            msg=f"seg_rows[0]={seg_rows[0]}",
        )
        self.assertAlmostEqual(
            sum(r["critical_path_ms"] for r in cp_thread_rows),
            300.0,
            places=2,
            msg=f"cp_thread_rows={cp_thread_rows}",
        )
        by_thread = {
            r["thread_name"]: r["critical_path_ms"] for r in cp_thread_rows
        }
        # wm_complete gets [390..400 ms] = 10.0 ms (not [360..400 ms] = 40.0 ms from first DrawFrame).
        self.assertAlmostEqual(
            by_thread["wm_complete"],
            10.0,
            places=2,
            msg=f"by_thread={by_thread}",
        )
        cp_slice_rows = sections[6].results
        cp_timeline_rows = sections[7].results
        assert cp_slice_rows is not None and cp_timeline_rows is not None
        self.assertTrue(
            any(r["slice_name"] == "bindApplication" for r in cp_slice_rows),
            msg=f"expected bindApplication in sections[6]={cp_slice_rows}",
        )
        self.assertTrue(
            any(
                "wakeup" in str(r.get("handoff_from", ""))
                for r in cp_timeline_rows
            ),
            msg=f"expected wakeup handoff in sections[7]={cp_timeline_rows}",
        )


if __name__ == "__main__":
    unittest.main()
