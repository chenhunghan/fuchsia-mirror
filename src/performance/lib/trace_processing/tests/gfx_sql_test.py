#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for metrics/_gfx_sql.py."""

import json
import tempfile
import unittest

import gfx_fxt_test_utils
from tp_shell import PerfettoTraceProcessor
from trace_processing.metrics import _gfx_sql
from trace_writing import fxt

_SCENIC_PID = gfx_fxt_test_utils.SCENIC_PID
_SCENIC_TID = gfx_fxt_test_utils.SCENIC_TID
_DISPLAY_PID = gfx_fxt_test_utils.DISPLAY_PID
_DISPLAY_TID = gfx_fxt_test_utils.DISPLAY_TID
_OTHER_PID = gfx_fxt_test_utils.OTHER_PID


def _run_flow_vsync(
    builder: fxt.Builder,
    event_name: str,
    *,
    min_ts_expr: str = "s.ts",
) -> list[tuple[int, int]]:
    """Materializes `_test_edges` and `_test_events` and returns `(event_ts, vsync_ts)` pairs."""
    setup_query = f"""\
{_gfx_sql.build_edges_table_sql("_test_edges")}

CREATE OR REPLACE PERFETTO TABLE _test_events AS
SELECT s.id, s.ts, ({min_ts_expr}) AS min_ts
FROM slice AS s
WHERE s.category = 'gfx'
  AND s.name = '{_gfx_sql.escape_sql_literal(event_name)}'
  AND s.dur > 0;
"""
    flow_query = _gfx_sql.build_flow_vsync_query(
        edges_table="_test_edges",
        events_table="_test_events",
        event_id_col="event_id",
        min_ts_col="min_ts",
    )
    with tempfile.NamedTemporaryFile(suffix=".fxt") as tmp:
        builder.write_to_file(tmp.name)
        with PerfettoTraceProcessor(trace_path=tmp.name) as tp:
            tp.run_query(setup_query)
            events = {
                int(row["id"]): int(row["ts"])
                for row in tp.run_query("SELECT id, ts FROM _test_events")
            }
            flows = tp.run_query(flow_query)
    return sorted(
        (events[int(row["event_id"])], int(row["vsync_ts"])) for row in flows
    )


class GfxSqlTest(unittest.TestCase):
    """Tests for shared Perfetto SQL graph and FPS helpers in _gfx_sql."""

    def test_out_of_scope_category_flows_and_parent_edges_ignored(self) -> None:
        """Ignores flows, parent-child edges, and dur=0 children outside gfx."""
        builder = gfx_fxt_test_utils.new_gfx_builder()
        builder.add_thread(_DISPLAY_PID, 3, "display-vsync")
        builder.add_thread(_DISPLAY_PID, 4, "input-bridge")

        # Case 1: dur=0 kernel:ipc ChannelMessage inside TestEvent -> kernel:ipc
        # -> gfx GpuWork -> OnVsync (130 us) is blocked; OnVsync (200 us) matches.
        builder.add_duration_complete(
            "gfx", "TestEvent", 100_000, 50_000, _SCENIC_PID, _SCENIC_TID
        )
        builder.add_duration_complete(
            "kernel:ipc",
            "ChannelMessage",
            110_000,
            0,
            _SCENIC_PID,
            _SCENIC_TID,
        )
        builder.add_flow_begin(
            "kernel:ipc", "ChannelFlow", 110_000, _SCENIC_PID, _SCENIC_TID, 2
        )
        builder.add_duration_complete(
            "gfx", "GpuWork", 112_000, 15_000, _DISPLAY_PID, _DISPLAY_TID
        )
        builder.add_duration_complete(
            "kernel:ipc",
            "ChannelMessage",
            115_000,
            0,
            _DISPLAY_PID,
            _DISPLAY_TID,
        )
        builder.add_flow_end(
            "kernel:ipc", "ChannelFlow", 115_000, _DISPLAY_PID, _DISPLAY_TID, 2
        )
        gfx_fxt_test_utils.add_vsync(builder, 130_000, dur_ns=10_000, tid=3)
        gfx_fxt_test_utils.add_vsync(builder, 200_000, dur_ns=10_000, tid=3)
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "Commit",
            3,
            _DISPLAY_PID,
            _DISPLAY_TID,
            113_000,
            _DISPLAY_PID,
            3,
            135_000,
        )
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "Dispatch",
            1,
            _SCENIC_PID,
            _SCENIC_TID,
            105_000,
            _DISPLAY_PID,
            3,
            205_000,
        )

        # Case 2: non-zero input child -> gfx grandchild -> OnVsync (320 us) is
        # blocked, and direct cross-category flows (gfx -> kernel:ipc -> gfx and
        # gfx -> input -> gfx) to 335, 350 us are ignored; OnVsync (400 us) matches.
        builder.add_duration_complete(
            "gfx", "TestEvent", 300_000, 50_000, _SCENIC_PID, _SCENIC_TID
        )
        builder.add_duration_complete(
            "input", "InputChild", 302_000, 8_000, _SCENIC_PID, _SCENIC_TID
        )
        builder.add_duration_complete(
            "gfx", "GfxGrandchild", 303_000, 4_000, _SCENIC_PID, _SCENIC_TID
        )
        builder.add_duration_complete(
            "kernel:ipc",
            "IpcBridge",
            310_000,
            20_000,
            _DISPLAY_PID,
            _DISPLAY_TID,
        )
        builder.add_duration_complete(
            "input", "InputBridge", 312_000, 18_000, _DISPLAY_PID, 4
        )
        gfx_fxt_test_utils.add_vsync(builder, 320_000, dur_ns=10_000, tid=3)
        gfx_fxt_test_utils.add_vsync(builder, 335_000, dur_ns=10_000, tid=3)
        gfx_fxt_test_utils.add_vsync(builder, 350_000, dur_ns=10_000, tid=3)
        gfx_fxt_test_utils.add_vsync(builder, 400_000, dur_ns=10_000, tid=3)
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "InputParentChildFlow",
            9,
            _SCENIC_PID,
            _SCENIC_TID,
            304_000,
            _DISPLAY_PID,
            3,
            322_000,
        )
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "CrossIpcIn",
            5,
            _SCENIC_PID,
            _SCENIC_TID,
            305_000,
            _DISPLAY_PID,
            _DISPLAY_TID,
            315_000,
        )
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "CrossIpcOut",
            6,
            _DISPLAY_PID,
            _DISPLAY_TID,
            320_000,
            _DISPLAY_PID,
            3,
            338_000,
        )
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "CrossInputIn",
            7,
            _SCENIC_PID,
            _SCENIC_TID,
            306_000,
            _DISPLAY_PID,
            4,
            316_000,
        )
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "CrossInputOut",
            8,
            _DISPLAY_PID,
            4,
            322_000,
            _DISPLAY_PID,
            3,
            355_000,
        )
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "ValidDispatch2",
            4,
            _SCENIC_PID,
            _SCENIC_TID,
            301_000,
            _DISPLAY_PID,
            3,
            405_000,
        )

        self.assertSequenceEqual(
            _run_flow_vsync(builder, "TestEvent"),
            ((100_000, 200_000), (300_000, 400_000)),
        )

    def test_duration_filter_boundaries_and_terminal_vsync_pruning(
        self,
    ) -> None:
        """Tests dur=0 instant, dur=-1 unterminated, terminal OnVsync pruning, and FALLBACK_VSYNC_QUERY."""
        builder = gfx_fxt_test_utils.new_gfx_builder()
        for tid in (3, 4, 5, 6):
            builder.add_thread(_DISPLAY_PID, tid, f"display-{tid}")
        builder.add_process(_OTHER_PID, "other")
        builder.add_thread(_OTHER_PID, 10, "unterminated-vsync")

        # Terminal OnVsync (90 us) encloses dur=0 instant reached by TestEvent 1;
        # its outgoing flows (to 250, 260 us) must be pruned.
        gfx_fxt_test_utils.add_vsync(builder, 90_000, dur_ns=30_000, tid=4)
        builder.add_flow_begin(
            "gfx", "FromTerminalVsync", 95_000, _DISPLAY_PID, 4, 4
        )
        builder.add_duration_complete(
            "gfx", "ChildOfTerminalVsync", 96_000, 5_000, _DISPLAY_PID, 4
        )
        builder.add_flow_begin(
            "gfx", "FromTerminalChild", 98_000, _DISPLAY_PID, 4, 5
        )
        builder.add_duration_complete(
            "gfx", "TestEvent", 100_000, 50_000, _SCENIC_PID, _SCENIC_TID
        )
        builder.add_flow_begin(
            "gfx", "ToInstant", 105_000, _SCENIC_PID, _SCENIC_TID, 1
        )
        builder.add_flow_begin(
            "gfx", "ToUnterminated", 106_000, _SCENIC_PID, _SCENIC_TID, 2
        )
        builder.add_flow_begin(
            "gfx", "ToTerminalOnVsync", 107_000, _SCENIC_PID, _SCENIC_TID, 6
        )
        builder.add_instant(
            "gfx", "TerminalVsyncInstant", 108_000, _DISPLAY_PID, 4
        )
        builder.add_flow_end(
            "gfx", "ToTerminalOnVsync", 108_000, _DISPLAY_PID, 4, 6
        )

        # Instant OnVsync (200 us) is non-terminal -> continues to complete OnVsync (300 us).
        builder.add_instant(
            "gfx",
            "Flatland::DisplayCompositor::OnVsync",
            200_000,
            _DISPLAY_PID,
            _DISPLAY_TID,
        )
        builder.add_flow_end(
            "gfx", "ToInstant", 200_000, _DISPLAY_PID, _DISPLAY_TID, 1
        )
        builder.add_flow_begin(
            "gfx", "FromInstant", 200_000, _DISPLAY_PID, _DISPLAY_TID, 3
        )

        # Unterminated OnVsync (250 us) reached by ToUnterminated.
        builder.add_duration_begin(
            "gfx",
            "Flatland::DisplayCompositor::OnVsync",
            250_000,
            _OTHER_PID,
            10,
        )
        builder.add_flow_end(
            "gfx", "ToUnterminated", 255_000, _OTHER_PID, 10, 2
        )
        gfx_fxt_test_utils.add_vsync(builder, 250_000, dur_ns=10_000, tid=5)
        builder.add_flow_end(
            "gfx", "FromTerminalVsync", 255_000, _DISPLAY_PID, 5, 4
        )
        gfx_fxt_test_utils.add_vsync(builder, 260_000, dur_ns=10_000, tid=6)
        builder.add_flow_end(
            "gfx", "FromTerminalChild", 265_000, _DISPLAY_PID, 6, 5
        )
        gfx_fxt_test_utils.add_vsync(builder, 300_000, dur_ns=10_000, tid=3)
        builder.add_flow_end("gfx", "FromInstant", 305_000, _DISPLAY_PID, 3, 3)

        self.assertSequenceEqual(
            _run_flow_vsync(builder, "TestEvent"),
            ((100_000, 300_000),),
        )
        with tempfile.NamedTemporaryFile(suffix=".fxt") as tmp:
            builder.write_to_file(tmp.name)
            with PerfettoTraceProcessor(trace_path=tmp.name) as tp:
                fallback_rows = tp.run_query(_gfx_sql.FALLBACK_VSYNC_QUERY)
        # Includes dur=0 at 200_000 and excludes dur=-1 at 250_000 on _OTHER_PID.
        self.assertSequenceEqual(
            [int(row["ts"]) for row in fallback_rows],
            (90_000, 200_000, 250_000, 260_000, 300_000),
        )

    def test_flow_vsync_before_min_ts_ignored_and_equality_included(
        self,
    ) -> None:
        """Enforces s.ts >= ev.min_ts (including equality and child->parent backward hops)."""
        builder = gfx_fxt_test_utils.new_gfx_builder()
        builder.add_thread(_DISPLAY_PID, 5, "frame")
        builder.add_thread(_DISPLAY_PID, 6, "display-vsync")

        # Event 1 (100 us) flows to dur=0 Receive inside Frame (50 us), whose
        # child->parent edge reaches EarlyCommit -> OnVsync (70 us < 100 us).
        builder.add_duration_complete(
            "gfx", "Frame", 50_000, 200_000, _DISPLAY_PID, 5
        )
        gfx_fxt_test_utils.add_vsync(builder, 70_000, dur_ns=10_000, tid=6)
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "EarlyCommit",
            1,
            _DISPLAY_PID,
            5,
            60_000,
            _DISPLAY_PID,
            6,
            75_000,
        )
        builder.add_duration_complete(
            "gfx", "TestEvent", 100_000, 50_000, _SCENIC_PID, _SCENIC_TID
        )
        builder.add_flow_begin(
            "gfx", "Dispatch", 105_000, _SCENIC_PID, _SCENIC_TID, 2
        )
        builder.add_instant("gfx", "Receive", 160_000, _DISPLAY_PID, 5)
        builder.add_flow_end("gfx", "Dispatch", 160_000, _DISPLAY_PID, 5, 2)
        gfx_fxt_test_utils.add_vsync(builder, 300_000, dur_ns=10_000, tid=6)
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "LateCommit",
            3,
            _DISPLAY_PID,
            5,
            170_000,
            _DISPLAY_PID,
            6,
            305_000,
        )

        # Event 2 (400 us) flows to OnVsync starting at exact equality (400 us).
        builder.add_duration_complete(
            "gfx", "TestEvent", 400_000, 10_000, _SCENIC_PID, _SCENIC_TID
        )
        gfx_fxt_test_utils.add_vsync(builder, 400_000, dur_ns=10_000, tid=6)
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "ExactEquality",
            4,
            _SCENIC_PID,
            _SCENIC_TID,
            402_000,
            _DISPLAY_PID,
            6,
            405_000,
        )

        # Event 3 (520 us) with min_ts = 500 us: flows to instant Receive2 inside
        # Frame2 (500 us), whose child->parent edge reaches OnVsync at 510 us
        # (500 <= 510 < 520), accepted by min_ts_col even though < s.ts.
        builder.add_duration_complete(
            "gfx", "Frame2", 500_000, 40_000, _DISPLAY_PID, 5
        )
        gfx_fxt_test_utils.add_vsync(builder, 510_000, dur_ns=20_000, tid=6)
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "CommitAfterMinTs",
            5,
            _DISPLAY_PID,
            5,
            505_000,
            _DISPLAY_PID,
            6,
            515_000,
        )
        builder.add_duration_complete(
            "gfx", "TestEvent", 520_000, 10_000, _SCENIC_PID, _SCENIC_TID
        )
        builder.add_flow_begin(
            "gfx",
            "PriorToSliceAfterMinTs",
            522_000,
            _SCENIC_PID,
            _SCENIC_TID,
            6,
        )
        builder.add_instant("gfx", "Receive2", 525_000, _DISPLAY_PID, 5)
        builder.add_flow_end(
            "gfx", "PriorToSliceAfterMinTs", 525_000, _DISPLAY_PID, 5, 6
        )

        self.assertSequenceEqual(
            _run_flow_vsync(
                builder,
                "TestEvent",
                min_ts_expr="CASE WHEN s.ts = 520000 THEN 500000 ELSE s.ts END",
            ),
            ((100_000, 300_000), (400_000, 400_000), (520_000, 510_000)),
        )

    def test_common_start_ctes_and_escape_sql_literal(self) -> None:
        """Computes MAX(MIN(ts)) across reachable upids, ignoring unconnected ones."""
        self.assertEqual(
            _gfx_sql.escape_sql_literal("app'one''two"), "app''one''''two"
        )

        builder = gfx_fxt_test_utils.new_gfx_builder()
        builder.add_process(_OTHER_PID, "unrelated")
        builder.add_thread(_OTHER_PID, 3, "unrelated-main")

        # Scenic starts at 10 us, Display starts at 50 us => common_start = 50 us.
        # Unrelated pid=3 starts at 500 us and must not affect common_start.
        builder.add_duration_complete(
            "gfx", "Event'A", 10_000, 5_000, _SCENIC_PID, _SCENIC_TID
        )
        gfx_fxt_test_utils.add_vsync(builder, 50_000)
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "InitFlow",
            1,
            _SCENIC_PID,
            _SCENIC_TID,
            12_000,
            _DISPLAY_PID,
            _DISPLAY_TID,
            51_000,
        )
        builder.add_duration_complete(
            "gfx", "UnrelatedWork", 500_000, 10_000, _OTHER_PID, 3
        )

        query = f"""\
{_gfx_sql.build_edges_table_sql("_cs_edges")}
WITH {_gfx_sql.build_common_start_ctes_sql("_cs_edges", "Event'A")}
SELECT start_ts FROM _common_start;
"""
        with tempfile.NamedTemporaryFile(suffix=".fxt") as tmp:
            builder.write_to_file(tmp.name)
            with PerfettoTraceProcessor(trace_path=tmp.name) as tp:
                rows = tp.run_query(query)

        self.assertEqual(len(rows), 1)
        self.assertEqual(int(rows[0]["start_ts"]), 50_000)

    def test_compute_fps_values(self) -> None:
        """Computes FPS from vsync deltas and skips squashed duplicate timestamps."""
        self.assertSequenceEqual(_gfx_sql.compute_fps_values([]), ())
        self.assertSequenceEqual(_gfx_sql.compute_fps_values([100_000]), ())
        self.assertSequenceEqual(
            _gfx_sql.compute_fps_values([100_000, 100_000]), ()
        )
        self.assertSequenceEqual(
            _gfx_sql.compute_fps_values([100_000, 100_000, 200_000, 400_000]),
            (10_000.0, 5_000.0),
        )

    def test_non_zero_child_does_not_climb_to_parent(self) -> None:
        """Restricts child->parent edges to dur=0 children so dur>0 children do not climb."""
        builder = gfx_fxt_test_utils.new_gfx_builder()
        builder.add_thread(_DISPLAY_PID, 3, "display-vsync")

        builder.add_duration_complete(
            "gfx", "TestEvent", 100_000, 50_000, _SCENIC_PID, _SCENIC_TID
        )
        builder.add_duration_complete(
            "gfx", "ParentFrame", 100_000, 80_000, _DISPLAY_PID, _DISPLAY_TID
        )
        builder.add_duration_complete(
            "gfx", "ChildWork", 110_000, 20_000, _DISPLAY_PID, _DISPLAY_TID
        )
        gfx_fxt_test_utils.add_vsync(builder, 150_000, dur_ns=10_000, tid=3)
        gfx_fxt_test_utils.add_vsync(builder, 200_000, dur_ns=10_000, tid=3)

        # Sibling flow from ParentFrame to earlier OnVsync (150 us).
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "ParentToEarlyVsync",
            1,
            _DISPLAY_PID,
            _DISPLAY_TID,
            105_000,
            _DISPLAY_PID,
            3,
            152_000,
        )
        # Flow from TestEvent lands on dur>0 ChildWork, which only flows to 200 us.
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "EventToChild",
            2,
            _SCENIC_PID,
            _SCENIC_TID,
            105_000,
            _DISPLAY_PID,
            _DISPLAY_TID,
            115_000,
        )
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "ChildToLateVsync",
            3,
            _DISPLAY_PID,
            _DISPLAY_TID,
            125_000,
            _DISPLAY_PID,
            3,
            202_000,
        )

        self.assertSequenceEqual(
            _run_flow_vsync(builder, "TestEvent"),
            ((100_000, 200_000),),
        )

    def test_common_start_skips_instant_and_unterminated_first_event(
        self,
    ) -> None:
        """Excludes dur=0 and dur=-1 first_slice_name slices when picking _first_event."""
        builder = gfx_fxt_test_utils.new_gfx_builder()
        builder.add_thread(_SCENIC_PID, 2, "scenic-unterminated")
        builder.add_thread(_SCENIC_PID, 3, "scenic-complete")
        builder.add_process(_OTHER_PID, "other")
        builder.add_thread(_OTHER_PID, 4, "other-main")

        # Early dur=0 and dur=-1 Event'A slices flow to _OTHER_PID starting at 500 us.
        builder.add_instant("gfx", "Event'A", 10_000, _SCENIC_PID, _SCENIC_TID)
        builder.add_duration_begin("gfx", "Event'A", 20_000, _SCENIC_PID, 2)
        builder.add_duration_complete(
            "gfx", "OtherWork", 500_000, 10_000, _OTHER_PID, 4
        )
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "InstantToOther",
            1,
            _SCENIC_PID,
            _SCENIC_TID,
            10_000,
            _OTHER_PID,
            4,
            502_000,
        )
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "UnterminatedToOther",
            2,
            _SCENIC_PID,
            2,
            21_000,
            _OTHER_PID,
            4,
            504_000,
        )

        # First complete (dur>0) Event'A at 30 us flows to _DISPLAY_PID starting at 50 us.
        builder.add_duration_complete(
            "gfx", "Event'A", 30_000, 5_000, _SCENIC_PID, 3
        )
        gfx_fxt_test_utils.add_vsync(builder, 50_000)
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "CompleteToDisplay",
            3,
            _SCENIC_PID,
            3,
            32_000,
            _DISPLAY_PID,
            _DISPLAY_TID,
            51_000,
        )

        query = f"""\
{_gfx_sql.build_edges_table_sql("_cs_edges")}
WITH {_gfx_sql.build_common_start_ctes_sql("_cs_edges", "Event'A")}
SELECT start_ts FROM _common_start;
"""
        with tempfile.NamedTemporaryFile(suffix=".fxt") as tmp:
            builder.write_to_file(tmp.name)
            with PerfettoTraceProcessor(trace_path=tmp.name) as tp:
                rows = tp.run_query(query)

        self.assertEqual(len(rows), 1)
        self.assertEqual(int(rows[0]["start_ts"]), 50_000)

    def test_common_start_includes_process_track_async_slices(self) -> None:
        """Includes async (process_track) gfx slices in _flow_upids and _upid_tracks."""
        # Case 1: _DISPLAY_PID is reached via OnVsync (50 us) and has an earlier
        # process-scoped async gfx slice on process_track (30 us), lowering
        # _common_start to 30 us.
        trace1 = {
            "displayTimeUnit": "ns",
            "traceEvents": [
                {
                    "cat": "gfx",
                    "name": "Event'A",
                    "ph": "X",
                    "pid": _SCENIC_PID,
                    "tid": _SCENIC_TID,
                    "ts": 10,
                    "dur": 5,
                },
                {
                    "cat": "gfx",
                    "name": "AsyncPrep",
                    "ph": "b",
                    "id2": {"local": "0x1"},
                    "pid": _DISPLAY_PID,
                    "tid": _DISPLAY_TID,
                    "ts": 30,
                },
                {
                    "cat": "gfx",
                    "name": "AsyncPrep",
                    "ph": "e",
                    "id2": {"local": "0x1"},
                    "pid": _DISPLAY_PID,
                    "tid": _DISPLAY_TID,
                    "ts": 35,
                },
                {
                    "cat": "gfx",
                    "name": _gfx_sql.DISPLAY_VSYNC_EVENT_NAME,
                    "ph": "X",
                    "pid": _DISPLAY_PID,
                    "tid": _DISPLAY_TID,
                    "ts": 50,
                    "dur": 10,
                },
                {
                    "cat": "gfx",
                    "name": "InitFlow",
                    "ph": "s",
                    "id": 1,
                    "pid": _SCENIC_PID,
                    "tid": _SCENIC_TID,
                    "ts": 12,
                },
                {
                    "cat": "gfx",
                    "name": "InitFlow",
                    "ph": "f",
                    "bp": "e",
                    "id": 1,
                    "pid": _DISPLAY_PID,
                    "tid": _DISPLAY_TID,
                    "ts": 51,
                },
            ],
        }

        query1 = f"""\
{_gfx_sql.build_edges_table_sql("_cs_edges1")}
WITH {_gfx_sql.build_common_start_ctes_sql("_cs_edges1", "Event'A")}
SELECT start_ts FROM _common_start;
"""
        with tempfile.NamedTemporaryFile(mode="w", suffix=".json") as tmp:
            json.dump(trace1, tmp)
            tmp.flush()
            with PerfettoTraceProcessor(trace_path=tmp.name) as tp:
                rows1 = tp.run_query(query1)

        self.assertEqual(len(rows1), 1)
        self.assertEqual(int(rows1[0]["start_ts"]), 30_000)

        # Case 2: _first_event itself is a process-scoped async (process_track)
        # slice on a process with no thread-track slices, exercising _flow_upids'
        # process_track join.
        trace2 = {
            "displayTimeUnit": "ns",
            "traceEvents": [
                {
                    "cat": "gfx",
                    "name": "AsyncStart",
                    "ph": "b",
                    "id2": {"local": "0x2"},
                    "pid": _OTHER_PID,
                    "tid": 1,
                    "ts": 40,
                },
                {
                    "cat": "gfx",
                    "name": "AsyncStart",
                    "ph": "e",
                    "id2": {"local": "0x2"},
                    "pid": _OTHER_PID,
                    "tid": 1,
                    "ts": 45,
                },
            ],
        }

        query2 = f"""\
{_gfx_sql.build_edges_table_sql("_cs_edges2")}
WITH {_gfx_sql.build_common_start_ctes_sql("_cs_edges2", "AsyncStart")}
SELECT start_ts FROM _common_start;
"""
        with tempfile.NamedTemporaryFile(mode="w", suffix=".json") as tmp:
            json.dump(trace2, tmp)
            tmp.flush()
            with PerfettoTraceProcessor(trace_path=tmp.name) as tp:
                rows2 = tp.run_query(query2)

        self.assertEqual(len(rows2), 1)
        self.assertEqual(int(rows2[0]["start_ts"]), 40_000)


if __name__ == "__main__":
    unittest.main()
