# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit and integration tests for the critical-path analyzer and plugin."""

from __future__ import annotations

import importlib.resources
import os
import tempfile
import unittest
from dataclasses import dataclass

from critical_path import (
    UNINSTRUMENTED_SLICE,
    CriticalPathPlugin,
    EdgeKind,
    EndpointSpec,
    ResolvedEndpoint,
    SliceGlobEndpoint,
    SliceIdEndpoint,
    SpineSegment,
    ThreadTimestampEndpoint,
    _build_pa_nodes_sql,
    compute_critical_path,
    merge_spine_segments,
    parse_cli_endpoint,
    resolve_endpoint,
)
from plugins import PluginArgumentError
from tp_shell import PerfettoTraceProcessor
from trace_writing import fxt

# FXT scheduler constants (see fuchsia.dev/reference/tracing/format/records).
IDLE_WEIGHT = -0x80000000
THREAD_BLOCKED = 3


def _write_temp_trace(builder: fxt.Builder) -> str:
    fd, path = tempfile.mkstemp(suffix=".fxt")
    os.close(fd)
    builder.write_to_file(path)
    return path


@dataclass(frozen=True)
class _CliEndpointCase:
    name: str
    cli_args: list[str]
    role: EdgeKind
    want: EndpointSpec | None = None
    want_err: str | None = None


class ParseCliEndpointTest(unittest.TestCase):
    """Tests for `parse_cli_endpoint` CLI selector validation and defaults."""

    def test_parse_cli_endpoint(self) -> None:
        parser = CriticalPathPlugin.build_parser()
        cases = [
            _CliEndpointCase(
                name="valid_start_slice_id_default_edge",
                cli_args=["--start-slice-id", "10"],
                role="start",
                want=SliceIdEndpoint(slice_id=10, edge="start"),
            ),
            _CliEndpointCase(
                name="valid_end_slice_glob_with_filters_default_edge",
                cli_args=[
                    "--end-slice",
                    "doFrame*",
                    "--end-process",
                    "*app*",
                    "--end-thread",
                    "main",
                    "--end-index",
                    "0",
                ],
                role="end",
                want=SliceGlobEndpoint(
                    slice_glob="doFrame*",
                    process_glob="*app*",
                    thread_glob="main",
                    occurrence_index=0,
                    edge="end",
                ),
            ),
            _CliEndpointCase(
                name="valid_ts_tid",
                cli_args=["--end-ts", "1000", "--end-tid", "42"],
                role="end",
                want=ThreadTimestampEndpoint(ts=1000, tid=42),
            ),
            _CliEndpointCase(
                name="valid_ts_utid",
                cli_args=["--end-ts", "1000", "--end-utid", "3"],
                role="end",
                want=ThreadTimestampEndpoint(ts=1000, utid=3),
            ),
            _CliEndpointCase(
                name="missing_all_selectors",
                cli_args=[],
                role="start",
                want_err="Missing start endpoint selector",
            ),
            _CliEndpointCase(
                name="conflicting_slice_id_and_glob",
                cli_args=["--start-slice-id", "1", "--start-slice", "foo"],
                role="start",
                want_err="Conflicting start endpoint selectors",
            ),
            _CliEndpointCase(
                name="conflicting_slice_and_ts",
                cli_args=[
                    "--end-slice",
                    "foo",
                    "--end-ts",
                    "100",
                    "--end-tid",
                    "1",
                ],
                role="end",
                want_err="Conflicting end endpoint selectors",
            ),
            _CliEndpointCase(
                name="slice_id_with_process_filter",
                cli_args=["--start-slice-id", "5", "--start-process", "app"],
                role="start",
                want_err="cannot be combined with --start-slice-id",
            ),
            _CliEndpointCase(
                name="ts_without_tid_or_utid",
                cli_args=["--start-ts", "500"],
                role="start",
                want_err="requires either --start-tid or --start-utid",
            ),
            _CliEndpointCase(
                name="tid_without_ts",
                cli_args=["--end-tid", "12"],
                role="end",
                want_err="requires --end-ts",
            ),
            _CliEndpointCase(
                name="both_tid_and_utid",
                cli_args=[
                    "--end-ts",
                    "500",
                    "--end-tid",
                    "12",
                    "--end-utid",
                    "2",
                ],
                role="end",
                want_err="either --end-tid or --end-utid, not both",
            ),
            _CliEndpointCase(
                name="edge_rejected_on_ts_endpoint",
                cli_args=[
                    "--start-ts",
                    "500",
                    "--start-tid",
                    "12",
                    "--start-edge",
                    "end",
                ],
                role="start",
                want_err="--start-edge is only valid with --start-slice or --start-slice-id",
            ),
        ]

        def check_case(tc: _CliEndpointCase) -> None:
            parsed = parser.parse_args(tc.cli_args)
            if tc.want_err is not None:
                with self.assertRaisesRegex(
                    PluginArgumentError,
                    tc.want_err,
                    msg=(
                        f"parse_cli_endpoint({tc.cli_args}, role={tc.role!r}) "
                        f"did not raise {tc.want_err!r}"
                    ),
                ):
                    parse_cli_endpoint(parsed, tc.role)
                return

            got = parse_cli_endpoint(parsed, tc.role)
            self.assertEqual(
                got,
                tc.want,
                msg=f"parse_cli_endpoint({tc.cli_args}, role={tc.role!r}) = {got}, want {tc.want}",
            )

        for tc in cases:
            with self.subTest(name=tc.name):
                check_case(tc)


class EndpointDataclassAndSpineTest(unittest.TestCase):
    """Tests for endpoint dataclasses and `merge_spine_segments`."""

    def test_thread_timestamp_endpoint_requires_one_id(self) -> None:
        with self.assertRaisesRegex(
            ValueError,
            "requires exactly one of tid or utid",
            msg="ThreadTimestampEndpoint(ts=100) should reject missing tid/utid",
        ):
            ThreadTimestampEndpoint(ts=100)

        with self.assertRaisesRegex(
            ValueError,
            "requires exactly one of tid or utid",
            msg="ThreadTimestampEndpoint(ts=100, tid=1, utid=2) should reject both tid and utid",
        ):
            ThreadTimestampEndpoint(ts=100, tid=1, utid=2)

    def test_resolved_endpoint_format_thread_label(self) -> None:
        ep_names_only = ResolvedEndpoint(
            utid=1,
            ts=1000,
            tid=None,
            thread_name="bar",
            upid=None,
            pid=None,
            process_name="foo",
        )
        got_names_only = ep_names_only.format_thread_label()
        self.assertEqual(
            got_names_only,
            "foo:bar",
            msg=f"({ep_names_only}).format_thread_label() = {got_names_only!r}",
        )

        ep_ids_only = ResolvedEndpoint(
            utid=2,
            ts=2000,
            tid=42,
            thread_name=None,
            upid=99,
            pid=99,
            process_name=None,
        )
        got_ids_only = ep_ids_only.format_thread_label()
        self.assertEqual(
            got_ids_only,
            "pid:99:tid:42",
            msg=f"({ep_ids_only}).format_thread_label() = {got_ids_only!r}",
        )

        ep_unknown = ResolvedEndpoint(
            utid=3,
            ts=3000,
            tid=None,
            thread_name=None,
            upid=None,
            pid=None,
            process_name=None,
        )
        got_unknown = ep_unknown.format_thread_label()
        self.assertEqual(
            got_unknown,
            "<unknown>:utid:3",
            msg=f"({ep_unknown}).format_thread_label() = {got_unknown!r}",
        )

    def test_merge_spine_segments(self) -> None:
        inputs = [
            SpineSegment(
                group_id=1,
                seg_index=0,
                utid=10,
                ts=100,
                dur=50,
                handoff_reason="a",
            ),
            SpineSegment(
                group_id=1,
                seg_index=1,
                utid=10,
                ts=150,
                dur=30,
                handoff_reason="b",
            ),
            SpineSegment(
                group_id=1,
                seg_index=2,
                utid=20,
                ts=180,
                dur=20,
                handoff_reason="c",
            ),
            SpineSegment(
                group_id=2,
                seg_index=0,
                utid=20,
                ts=200,
                dur=40,
                handoff_reason="d",
            ),
        ]
        got = merge_spine_segments(inputs)
        want = [
            SpineSegment(
                group_id=1,
                seg_index=0,
                utid=10,
                ts=100,
                dur=80,
                handoff_reason="a",
            ),
            SpineSegment(
                group_id=1,
                seg_index=1,
                utid=20,
                ts=180,
                dur=20,
                handoff_reason="c",
            ),
            SpineSegment(
                group_id=2,
                seg_index=0,
                utid=20,
                ts=200,
                dur=40,
                handoff_reason="d",
            ),
        ]
        self.assertEqual(
            got,
            want,
            msg=f"merge_spine_segments({inputs}) = {got}, want {want}",
        )


class CriticalPathPluginEndToEndTest(unittest.TestCase):
    """End-to-end tests exercising `critical_path.py` against real PerfettoTraceProcessor FXT traces."""

    def _build_wakeup_and_flow_trace(self) -> str:
        """Creates a synthetic FXT trace with wakeup blockers, nested slices, flows, and async slices.

        Timeline (in milliseconds, `1 ms = 1_000_000 ns`):
          - Process 100 (`app_proc`): Thread 101 (`app_main`), Thread 102 (`RenderThread`)
          - Process 200 (`sys_proc`): Thread 201 (`ipc_worker`), Thread 202 (`event_loop`)
          - Process 300 (`other_proc`): Thread 301 (`isolated_worker`)

          1. `t = 10 ms`: `app_main` (101) wakes & starts `Running` on CPU 0.
             - `app_main` opens outer slice `doWork` `[10 ms, 50 ms]` and child slice
               `prepare` `[12 ms, 18 ms]` (leaving `[10..12]` and `[18..20]` to outer `doWork`).
          2. `t = 20 ms`: `app_main` wakes `ipc_worker` (201) and blocks (`S`) `[20 ms, 35 ms]`.
             - `ipc_worker` stays in `W` `[20 ms, 22 ms]`, then runs on CPU 1 `[22 ms, 35 ms]`
               with slice `HandleRpc` `[23 ms, 34 ms]` (and uninstrumented `[22..23]`, `[34..35]`).
          3. `t = 35 ms`: `ipc_worker` wakes `app_main` (`W` `[35 ms, 36 ms]`, `Running` `[36 ms, 50 ms]`).
          4. `t = 50 ms`: `app_main` closes `doWork`, wakes `RenderThread` (102), and blocks.
             - `RenderThread` runs `[50 ms, 70 ms]` executing `DrawFrames` `[50 ms, 70 ms]`.
          5. Meanwhile `event_loop` (202) was ALREADY `Running` on CPU 2 since `t = 45 ms`!
             - At `t = 65 ms`, `RenderThread` emits `FlowBegin(id=99)` inside `SubmitFrame` `[64..66 ms]`.
             - At `t = 72 ms`, `event_loop` (still continuously `Running` `[45 ms, 90 ms]` with no new
               scheduler wakeup!) receives `FlowEnd(id=99)` inside `PresentFrame` `[72 ms, 85 ms]`.
          6. `isolated_worker` (301) runs `[15 ms, 88 ms]` with slice `UnrelatedTask` `[20 ms, 80 ms]`
             with zero wakeups or flows connecting it to `app_proc` or `sys_proc`.
          7. Async slice `AsyncLaunch` `[10 ms, 85 ms]` on `app_proc` (`utid IS NULL`).
        """
        ms = 1_000_000
        b = fxt.Builder()
        b.add_process(100, "app_proc")
        b.add_thread(100, 101, "app_main")
        b.add_thread(100, 102, "RenderThread")

        b.add_process(200, "sys_proc")
        b.add_thread(200, 201, "ipc_worker")
        b.add_thread(200, 202, "event_loop")

        b.add_process(300, "other_proc")
        b.add_thread(300, 301, "isolated_worker")

        idle_tid = 9999

        # Async non-thread slice on pid=100
        b.add_async_begin("app", "AsyncLaunch", 10 * ms, 100, 101, async_id=7)
        b.add_async_end("app", "AsyncLaunch", 85 * ms, 100, 101, async_id=7)

        # Unrelated isolated_worker on CPU 3: [15 ms, 88 ms]
        b.add_thread_wakeup(14 * ms, 3, 301)
        b.add_context_switch(
            15 * ms,
            3,
            THREAD_BLOCKED,
            idle_tid,
            301,
            outgoing_priority=IDLE_WEIGHT,
        )
        b.add_duration_begin("bg", "UnrelatedTask", 20 * ms, 300, 301)
        b.add_duration_end("bg", "UnrelatedTask", 80 * ms, 300, 301)
        b.add_context_switch(
            88 * ms,
            3,
            THREAD_BLOCKED,
            301,
            idle_tid,
            incoming_priority=IDLE_WEIGHT,
        )

        # 1. app_main wakes at 9 ms, runs [10 ms, 20 ms] on CPU 0
        b.add_thread_wakeup(9 * ms, 0, 101)
        b.add_context_switch(
            10 * ms,
            0,
            THREAD_BLOCKED,
            idle_tid,
            101,
            outgoing_priority=IDLE_WEIGHT,
        )
        b.add_duration_begin("ui", "doWork", 10 * ms, 100, 101)
        b.add_duration_begin("ui", "prepare", 12 * ms, 100, 101)
        b.add_duration_end("ui", "prepare", 18 * ms, 100, 101)

        # 2. At 20 ms, app_main wakes ipc_worker (201) and sleeps
        b.add_thread_wakeup(20 * ms, 1, 201, waker_tid=101)
        b.add_context_switch(
            20 * ms,
            0,
            THREAD_BLOCKED,
            101,
            idle_tid,
            incoming_priority=IDLE_WEIGHT,
        )
        # ipc_worker waits in W [20..22 ms], then runs [22..35 ms] on CPU 1
        b.add_context_switch(
            22 * ms,
            1,
            THREAD_BLOCKED,
            idle_tid,
            201,
            outgoing_priority=IDLE_WEIGHT,
        )
        b.add_duration_begin("ipc", "HandleRpc", 23 * ms, 200, 201)
        b.add_duration_end("ipc", "HandleRpc", 34 * ms, 200, 201)

        # 3. At 35 ms, ipc_worker wakes app_main (101) and sleeps
        b.add_thread_wakeup(35 * ms, 0, 101, waker_tid=201)
        b.add_context_switch(
            35 * ms,
            1,
            THREAD_BLOCKED,
            201,
            idle_tid,
            incoming_priority=IDLE_WEIGHT,
        )
        # app_main in W [35..36 ms], then runs [36..50 ms] on CPU 0
        b.add_context_switch(
            36 * ms,
            0,
            THREAD_BLOCKED,
            idle_tid,
            101,
            outgoing_priority=IDLE_WEIGHT,
        )
        b.add_duration_end("ui", "doWork", 50 * ms, 100, 101)

        # event_loop (202) starts running on CPU 2 at 45 ms (already awake before flow!)
        b.add_thread_wakeup(44 * ms, 2, 202)
        b.add_context_switch(
            45 * ms,
            2,
            THREAD_BLOCKED,
            idle_tid,
            202,
            outgoing_priority=IDLE_WEIGHT,
        )

        # 4. At 50 ms, app_main wakes RenderThread (102) and sleeps
        b.add_thread_wakeup(50 * ms, 1, 102, waker_tid=101)
        b.add_context_switch(
            50 * ms,
            0,
            THREAD_BLOCKED,
            101,
            idle_tid,
            incoming_priority=IDLE_WEIGHT,
        )
        b.add_context_switch(
            50 * ms,
            1,
            THREAD_BLOCKED,
            idle_tid,
            102,
            outgoing_priority=IDLE_WEIGHT,
        )
        b.add_duration_begin("gfx", "DrawFrames", 50 * ms, 100, 102)
        b.add_duration_begin("gfx", "SubmitFrame", 64 * ms, 100, 102)
        b.add_flow_begin("gfx", "frame_flow", 65 * ms, 100, 102, flow_id=99)
        b.add_duration_end("gfx", "SubmitFrame", 66 * ms, 100, 102)
        b.add_duration_end("gfx", "DrawFrames", 70 * ms, 100, 102)
        b.add_context_switch(
            70 * ms,
            1,
            THREAD_BLOCKED,
            102,
            idle_tid,
            incoming_priority=IDLE_WEIGHT,
        )

        # 5. event_loop (202) receives flow 99 inside PresentFrame [72..85 ms]
        b.add_duration_begin("gfx", "PresentFrame", 72 * ms, 200, 202)
        b.add_flow_end("gfx", "frame_flow", 72 * ms, 200, 202, flow_id=99)
        b.add_duration_end("gfx", "PresentFrame", 85 * ms, 200, 202)
        b.add_context_switch(
            90 * ms,
            2,
            THREAD_BLOCKED,
            202,
            idle_tid,
            incoming_priority=IDLE_WEIGHT,
        )

        trace_path = _write_temp_trace(b)
        self.addCleanup(self._remove_file_if_exists, trace_path)
        return trace_path

    @staticmethod
    def _remove_file_if_exists(path: str) -> None:
        if os.path.exists(path):
            os.remove(path)

    def test_single_thread_critical_path_expands_wakeup_blocker_and_leaf_slices(
        self,
    ) -> None:
        """Verifies single-thread critical path over `doWork` `[10 ms, 50 ms]`."""
        trace_path = self._build_wakeup_and_flow_trace()

        plugin = CriticalPathPlugin()
        args = [
            "--start-slice",
            "doWork",
            "--start-edge",
            "start",
            "--end-slice",
            "doWork",
            "--end-edge",
            "end",
        ]
        sections = plugin.analyze(args, trace_path)
        self.assertEqual(
            len(sections),
            3,
            msg=f"plugin.analyze({args}) returned {len(sections)} sections, want 3",
        )
        for sec in sections:
            self.assertIsNone(
                sec.error,
                msg=f"section {sec.name!r} had unexpected error for args={args}: {sec.error}",
            )

        summary_rows = sections[0].results
        slice_rows = sections[1].results
        timeline_rows = sections[2].results
        assert summary_rows is not None
        assert slice_rows is not None
        assert timeline_rows is not None

        # Total window = 40.0 ms:
        #   - app_main: [10..20] Running (10 ms) + [35..36] W (1 ms) + [36..50] Running (14 ms) = 25.0 ms
        #   - ipc_worker: [20..22] W (2 ms) + [22..35] Running (13 ms) = 15.0 ms
        by_thread = {r["thread_name"]: r for r in summary_rows}
        self.assertEqual(
            set(by_thread.keys()),
            {"app_main", "ipc_worker"},
            msg=f"summary threads for {args} = {set(by_thread.keys())}",
        )
        self.assertAlmostEqual(
            by_thread["app_main"]["critical_path_ms"],
            25.0,
            places=2,
            msg=f"app_main row={by_thread['app_main']}",
        )
        self.assertAlmostEqual(
            by_thread["app_main"]["running_ms"],
            24.0,
            places=2,
            msg=f"app_main row={by_thread['app_main']}",
        )
        self.assertAlmostEqual(
            by_thread["app_main"]["waking_ms"],
            1.0,
            places=2,
            msg=f"app_main row={by_thread['app_main']}",
        )
        self.assertAlmostEqual(
            by_thread["ipc_worker"]["critical_path_ms"],
            15.0,
            places=2,
            msg=f"ipc_worker row={by_thread['ipc_worker']}",
        )
        self.assertAlmostEqual(
            by_thread["ipc_worker"]["running_ms"],
            13.0,
            places=2,
            msg=f"ipc_worker row={by_thread['ipc_worker']}",
        )
        self.assertAlmostEqual(
            by_thread["ipc_worker"]["waking_ms"],
            2.0,
            places=2,
            msg=f"ipc_worker row={by_thread['ipc_worker']}",
        )
        self.assertEqual(
            by_thread["app_main"]["spans"],
            3,
            msg=f"app_main spans in row={by_thread['app_main']}",
        )
        self.assertEqual(
            by_thread["ipc_worker"]["spans"],
            2,
            msg=f"ipc_worker spans in row={by_thread['ipc_worker']}",
        )
        self.assertEqual(
            by_thread["app_main"]["unknown_ms"],
            0.0,
            msg=f"app_main unknown_ms in row={by_thread['app_main']}",
        )

        # Leaf slice self-time conservation across 40.0 ms:
        #   - doWork (self): [10..12] (2) + [18..20] (2) + [35..50] (15) = 19.0 ms (1 slice_id)
        #   - prepare (nested child): [12..18] = 6.0 ms (1 slice_id)
        #   - HandleRpc: [23..34] = 11.0 ms (1 slice_id)
        #   - <uninstrumented> on ipc_worker: [20..23] (3) + [34..35] (1) = 4.0 ms (2 runs)
        by_slice_row = {
            (r["thread_name"], r["slice_name"]): r for r in slice_rows
        }
        self.assertAlmostEqual(
            by_slice_row[("app_main", "doWork")]["critical_path_ms"],
            19.0,
            places=2,
            msg=f"slice breakdown={by_slice_row}",
        )
        self.assertEqual(
            by_slice_row[("app_main", "doWork")]["occurrences"],
            1,
            msg=f"doWork occurrences in {by_slice_row}",
        )
        self.assertAlmostEqual(
            by_slice_row[("app_main", "prepare")]["critical_path_ms"],
            6.0,
            places=2,
            msg=f"slice breakdown={by_slice_row}",
        )
        self.assertEqual(
            by_slice_row[("app_main", "prepare")]["occurrences"],
            1,
            msg=f"prepare occurrences in {by_slice_row}",
        )
        self.assertAlmostEqual(
            by_slice_row[("ipc_worker", "HandleRpc")]["critical_path_ms"],
            11.0,
            places=2,
            msg=f"slice breakdown={by_slice_row}",
        )
        self.assertEqual(
            by_slice_row[("ipc_worker", "HandleRpc")]["occurrences"],
            1,
            msg=f"HandleRpc occurrences in {by_slice_row}",
        )
        self.assertAlmostEqual(
            by_slice_row[("ipc_worker", UNINSTRUMENTED_SLICE)][
                "critical_path_ms"
            ],
            4.0,
            places=2,
            msg=f"slice breakdown={by_slice_row}",
        )
        self.assertEqual(
            by_slice_row[("ipc_worker", UNINSTRUMENTED_SLICE)]["occurrences"],
            2,
            msg=f"uninstrumented occurrences in {by_slice_row}",
        )
        self.assertAlmostEqual(
            sum(r["dur_ms"] for r in timeline_rows),
            40.0,
            places=2,
            msg=f"timeline={timeline_rows}",
        )

    def test_cross_thread_wakeup_spine_between_slices(self) -> None:
        """Verifies Tier-1 cross-thread wakeup handoff from `app_main` `doWork` to `RenderThread` `DrawFrames`."""
        trace_path = self._build_wakeup_and_flow_trace()

        plugin = CriticalPathPlugin()
        args = ["--start-slice", "doWork", "--end-slice", "DrawFrames"]
        sections = plugin.analyze(args, trace_path)
        for sec in sections:
            self.assertIsNone(
                sec.error,
                msg=f"section {sec.name!r} errored for {args}: {sec.error}",
            )

        summary_rows = sections[0].results
        assert summary_rows is not None
        total_ms = sum(r["critical_path_ms"] for r in summary_rows)
        # [10 ms .. 70 ms] = 60.0 ms across app_main (25), RenderThread (20), ipc_worker (15)
        self.assertAlmostEqual(
            total_ms,
            60.0,
            places=2,
            msg=f"summary_rows for {args} = {summary_rows}",
        )
        by_thread = {
            r["thread_name"]: r["critical_path_ms"] for r in summary_rows
        }
        self.assertAlmostEqual(by_thread["app_main"], 25.0, places=2)
        self.assertAlmostEqual(by_thread["RenderThread"], 20.0, places=2)
        self.assertAlmostEqual(by_thread["ipc_worker"], 15.0, places=2)

    def test_cross_thread_flow_handoff_to_already_running_server(self) -> None:
        """Verifies Tier-2 time-monotonic priority BFS bridges `flow` edges to an awake thread."""
        trace_path = self._build_wakeup_and_flow_trace()

        plugin = CriticalPathPlugin()
        args = ["--start-slice", "doWork", "--end-slice", "PresentFrame"]
        sections = plugin.analyze(args, trace_path)
        for sec in sections:
            self.assertIsNone(
                sec.error,
                msg=f"section {sec.name!r} errored for {args}: {sec.error}",
            )

        summary_rows = sections[0].results
        slice_rows = sections[1].results
        timeline_rows = sections[2].results
        assert summary_rows is not None
        assert slice_rows is not None
        assert timeline_rows is not None
        # [10 ms .. 85 ms] = 75.0 ms, handing off to event_loop at SubmitFrame end (66 ms).
        total_ms = sum(r["critical_path_ms"] for r in summary_rows)
        self.assertAlmostEqual(
            total_ms,
            75.0,
            places=2,
            msg=f"summary_rows for {args} = {summary_rows}",
        )
        by_thread = {
            r["thread_name"]: r["critical_path_ms"] for r in summary_rows
        }
        self.assertAlmostEqual(
            by_thread["app_main"],
            25.0,
            places=2,
            msg=f"by_thread for {args} = {by_thread}",
        )
        self.assertAlmostEqual(
            by_thread["ipc_worker"],
            15.0,
            places=2,
            msg=f"by_thread for {args} = {by_thread}",
        )
        self.assertAlmostEqual(
            by_thread["RenderThread"],
            16.0,
            places=2,
            msg=f"by_thread for {args} = {by_thread}",
        )
        self.assertAlmostEqual(
            by_thread["event_loop"],
            19.0,
            places=2,
            msg=f"by_thread for {args} = {by_thread}",
        )
        by_slice = {
            (r["thread_name"], r["slice_name"]): r["critical_path_ms"]
            for r in slice_rows
        }
        self.assertAlmostEqual(
            by_slice[("RenderThread", "DrawFrames")],
            14.0,
            places=2,
            msg=f"by_slice for {args} = {by_slice}",
        )
        self.assertAlmostEqual(
            by_slice[("RenderThread", "SubmitFrame")],
            2.0,
            places=2,
            msg=f"by_slice for {args} = {by_slice}",
        )
        self.assertAlmostEqual(
            by_slice[("event_loop", "PresentFrame")],
            13.0,
            places=2,
            msg=f"by_slice for {args} = {by_slice}",
        )
        flow_handoffs = [
            r for r in timeline_rows if "flow" in str(r.get("handoff_from", ""))
        ]
        self.assertTrue(
            flow_handoffs,
            msg=f"expected flow handoff annotation in timeline={timeline_rows}",
        )

    def test_disconnected_cross_thread_endpoints_return_diagnostic_error(
        self,
    ) -> None:
        """Verifies disconnected threads emit an explicit causal-disconnect error."""
        trace_path = self._build_wakeup_and_flow_trace()

        plugin = CriticalPathPlugin()
        args = ["--start-slice", "doWork", "--end-slice", "UnrelatedTask"]
        sections = plugin.analyze(args, trace_path)
        self.assertEqual(len(sections), 3)
        for sec in sections:
            assert sec.error is not None
            self.assertIn(
                "No causal scheduler wakeup or trace flow path connects",
                sec.error,
                msg=f"unexpected error for disconnected {args}: {sec.error}",
            )

    def test_async_non_thread_slice_rejected_clearly(self) -> None:
        """Verifies async/process-track slices (`utid IS NULL`) are rejected by glob and ID."""
        trace_path = self._build_wakeup_and_flow_trace()

        plugin = CriticalPathPlugin()
        glob_args = ["--start-slice", "AsyncLaunch", "--end-slice", "doWork"]
        sections = plugin.analyze(glob_args, trace_path)
        assert sections[0].error is not None
        self.assertIn(
            "no utid",
            sections[0].error,
            msg=f"glob_args={glob_args} error={sections[0].error}",
        )

        with PerfettoTraceProcessor(trace_path) as tp:
            async_rows = tp.run_query(
                "SELECT id FROM slice WHERE name = 'AsyncLaunch' LIMIT 1;"
            )
            async_id = int(async_rows[0]["id"])
            db_objects = tp.get_tables()
            with self.assertRaisesRegex(
                Exception,
                "no utid",
                msg=f"resolve_endpoint(slice_id={async_id}) should reject async slice",
            ):
                resolve_endpoint(
                    tp,
                    db_objects,
                    SliceIdEndpoint(slice_id=async_id),
                    role="start",
                )

    def test_end_slice_glob_picks_earliest_end_after_start(self) -> None:
        """Verifies `--end-edge end` picks the first slice to end, not to start."""
        ms = 1_000_000
        trace_path = self._build_wakeup_and_flow_trace()
        with PerfettoTraceProcessor(trace_path) as tp:
            db_objects = tp.get_tables()
            # `doWork` [10, 50] starts first, but `prepare` [12, 18] ends first.
            got = resolve_endpoint(
                tp,
                db_objects,
                SliceGlobEndpoint(
                    slice_glob="[dp]*", thread_glob="app_main", edge="end"
                ),
                role="end",
                min_ts=11 * ms,
            )
        self.assertEqual(
            got.ts,
            18 * ms,
            msg=f"resolve_endpoint('[dp]*', edge=end, min_ts=11 ms) = {got}",
        )

    def test_explicit_tid_ts_and_no_coalesce(self) -> None:
        """Verifies explicit `--start-tid`/`--start-ts` selectors and `--no-coalesce`."""
        trace_path = self._build_wakeup_and_flow_trace()

        plugin = CriticalPathPlugin()
        coalesced = plugin.analyze(
            [
                "--start-tid",
                "101",
                "--start-ts",
                "10000000",
                "--end-tid",
                "101",
                "--end-ts",
                "50000000",
            ],
            trace_path,
        )
        uncoalesced = plugin.analyze(
            [
                "--start-tid",
                "101",
                "--start-ts",
                "10000000",
                "--end-tid",
                "101",
                "--end-ts",
                "50000000",
                "--no-coalesce",
            ],
            trace_path,
        )
        c_tl = coalesced[2].results
        u_tl = uncoalesced[2].results
        assert c_tl is not None
        assert u_tl is not None
        self.assertGreaterEqual(len(u_tl), len(c_tl))
        self.assertAlmostEqual(
            sum(r["dur_ms"] for r in u_tl),
            40.0,
            places=2,
        )

        trunc = plugin.analyze(
            [
                "--start-tid",
                "101",
                "--start-ts",
                "10000000",
                "--end-tid",
                "101",
                "--end-ts",
                "50000000",
                "--no-coalesce",
                "--timeline-limit",
                "1",
            ],
            trace_path,
        )
        self.assertIn(
            "Showing first 1 of",
            trunc[2].note or "",
            msg=f"expected truncation note on timeline section, got {trunc[2].note!r}",
        )

    def test_inverted_time_range_and_out_of_range_index_errors(self) -> None:
        """Verifies inverted endpoint timestamps and out-of-range --start-index return errors."""
        trace_path = self._build_wakeup_and_flow_trace()
        plugin = CriticalPathPlugin()
        inv_args = [
            "--start-tid",
            "101",
            "--start-ts",
            "50000000",
            "--end-tid",
            "101",
            "--end-ts",
            "10000000",
        ]
        inv_sections = plugin.analyze(inv_args, trace_path)
        for sec in inv_sections:
            assert sec.error is not None
            self.assertIn(
                "must be greater than start point",
                sec.error,
                msg=f"inv_args={inv_args} sec={sec}",
            )

        idx_args = [
            "--start-slice",
            "doWork",
            "--start-index",
            "5",
            "--end-slice",
            "DrawFrames",
        ]
        idx_sections = plugin.analyze(idx_args, trace_path)
        for sec in idx_sections:
            assert sec.error is not None
            self.assertIn(
                "out of range",
                sec.error,
                msg=f"idx_args={idx_args} sec={sec}",
            )

    def test_non_positive_limits_raise_argument_error(self) -> None:
        """Verifies --limit <= 0 and --timeline-limit <= 0 raise PluginArgumentError."""
        plugin = CriticalPathPlugin()
        for flag, val in (
            ("--limit", "0"),
            ("--limit", "-1"),
            ("--timeline-limit", "0"),
        ):
            bad_args = [
                "--start-tid",
                "101",
                "--start-ts",
                "10",
                "--end-tid",
                "101",
                "--end-ts",
                "20",
                flag,
                val,
            ]
            with self.assertRaisesRegex(
                PluginArgumentError,
                "positive integer",
                msg=f"bad_args={bad_args}",
            ):
                plugin.analyze(bad_args, "dummy_trace")

    def test_sample_fxt_slice_only_fallback(self) -> None:
        """Verifies `compute_critical_path` conserves duration even when `thread_state` has 0 rows."""
        source = importlib.resources.files("test_data").joinpath(
            "sample_fxt.fxt"
        )
        with importlib.resources.as_file(source) as trace_path:
            with PerfettoTraceProcessor(str(trace_path)) as tp:
                db_objects = tp.get_tables()
                rows = tp.run_query(
                    """
                    SELECT s.id, s.ts, s.dur, tt.utid
                    FROM slice s
                    JOIN thread_track tt ON s.track_id = tt.id
                    WHERE s.dur > 0
                    ORDER BY s.dur DESC
                    LIMIT 1;
                    """
                )
                self.assertTrue(rows)
                r = rows[0]
                bd = compute_critical_path(
                    tp,
                    db_objects,
                    [
                        SpineSegment(
                            group_id=1,
                            seg_index=0,
                            utid=int(r["utid"]),
                            ts=int(r["ts"]),
                            dur=int(r["dur"]),
                        )
                    ],
                )
                self.assertIn(1, bd)
                self.assertEqual(bd[1].total_dur_ns, int(r["dur"]))
                self.assertEqual(
                    bd[1].thread_summary[0]["unattributed_sleep_ms"],
                    0.0,
                    msg=f"slice-only fallback thread_summary={bd[1].thread_summary}",
                )
                self.assertGreater(
                    bd[1].thread_summary[0]["unknown_ms"],
                    0.0,
                    msg=f"slice-only fallback thread_summary={bd[1].thread_summary}",
                )

    def test_open_ended_running_threads_unclosed_slices_and_sleeping_endpoints(
        self,
    ) -> None:
        """Verifies EOF `dur=-1` threads/slices, self-wakers, and sleeping endpoints."""
        ms = 1_000_000
        b = fxt.Builder()
        b.add_process(500, "boot_proc")
        b.add_thread(500, 501, "boot_waker")
        b.add_thread(500, 502, "eof_target")
        b.add_thread(500, 503, "timer_worker")
        idle_tid = 9999

        # boot_waker (501) starts Running at 5 ms without any prior wakeup and never switches out (dur=-1).
        b.add_context_switch(
            5 * ms,
            0,
            THREAD_BLOCKED,
            idle_tid,
            501,
            outgoing_priority=IDLE_WEIGHT,
        )
        # eof_target (502) runs [8..10 ms] on CPU 1, then sleeps [10..20 ms].
        b.add_context_switch(
            8 * ms,
            1,
            THREAD_BLOCKED,
            idle_tid,
            502,
            outgoing_priority=IDLE_WEIGHT,
        )
        b.add_context_switch(
            10 * ms,
            1,
            THREAD_BLOCKED,
            502,
            idle_tid,
            incoming_priority=IDLE_WEIGHT,
        )
        # At 20 ms, boot_waker (501, dur=-1) wakes eof_target (502); 502 runs [21..30 ms], sleeps [30..40 ms].
        b.add_thread_wakeup(20 * ms, 1, 502, waker_tid=501)
        b.add_context_switch(
            21 * ms,
            1,
            THREAD_BLOCKED,
            idle_tid,
            502,
            outgoing_priority=IDLE_WEIGHT,
        )
        b.add_context_switch(
            30 * ms,
            1,
            THREAD_BLOCKED,
            502,
            idle_tid,
            incoming_priority=IDLE_WEIGHT,
        )
        # At 30 ms, timer_worker (503) wakes via IRQ/timer (waker_tid=None -> self-waker in FXT),
        # waits in W [30..33 ms], runs [33..40 ms] on CPU 2, and wakes eof_target (502) at 40 ms.
        b.add_thread_wakeup(30 * ms, 2, 503)
        b.add_context_switch(
            33 * ms,
            2,
            THREAD_BLOCKED,
            idle_tid,
            503,
            outgoing_priority=IDLE_WEIGHT,
        )
        b.add_thread_wakeup(40 * ms, 1, 502, waker_tid=503)
        b.add_context_switch(
            40 * ms,
            2,
            THREAD_BLOCKED,
            503,
            idle_tid,
            incoming_priority=IDLE_WEIGHT,
        )
        # eof_target (502) switches onto CPU 1 at 41 ms, opens UnclosedTail at 42 ms (dur=-1),
        # emits DoneInstant at 55 ms, and stays Running until EOF (thread_state.dur=-1).
        b.add_context_switch(
            41 * ms,
            1,
            THREAD_BLOCKED,
            idle_tid,
            502,
            outgoing_priority=IDLE_WEIGHT,
        )
        b.add_duration_begin("app", "UnclosedTail", 42 * ms, 500, 502)
        b.add_instant("app", "DoneInstant", 55 * ms, 500, 502)

        trace_path = _write_temp_trace(b)
        self.addCleanup(self._remove_file_if_exists, trace_path)
        plugin = CriticalPathPlugin()

        # 1. Anchoring on the end edge of an incomplete (dur=-1) slice is explicitly rejected.
        unclosed_args = [
            "--start-tid",
            "501",
            "--start-ts",
            "10000000",
            "--end-slice",
            "UnclosedTail",
            "--end-edge",
            "end",
        ]
        unclosed_res = plugin.analyze(unclosed_args, trace_path)
        assert unclosed_res[0].error is not None
        self.assertIn(
            "incomplete (dur=-1)",
            unclosed_res[0].error,
            msg=f"unclosed_args={unclosed_args} error={unclosed_res[0].error}",
        )

        # 2. Cross-thread wakeup from dur=-1 bootstrap waker (501) to dur=-1 target (502) succeeds,
        # attributes UnclosedTail [42..55 ms] = 13.0 ms, and never emits self-wakeup on timer_worker.
        eof_args = [
            "--start-tid",
            "501",
            "--start-ts",
            "10000000",
            "--end-slice",
            "DoneInstant",
        ]
        eof_res = plugin.analyze(eof_args, trace_path)
        for sec in eof_res:
            self.assertIsNone(
                sec.error,
                msg=f"section {sec.name!r} errored for {eof_args}: {sec.error}",
            )
        summary_rows = eof_res[0].results
        slice_rows = eof_res[1].results
        timeline_rows = eof_res[2].results
        assert summary_rows is not None
        assert slice_rows is not None
        assert timeline_rows is not None
        # Tier 1 falls back across `prev_id` past the dead-end IRQ waker (503) without
        # triggering a Tier-2 `flow` query or mislabeling the chain `wakeup+flow`.
        self.assertIn(
            "cross-thread wakeup spine",
            eof_res[0].note or "",
            msg=f"eof_res[0].note={eof_res[0].note!r} for {eof_args}",
        )

        self.assertAlmostEqual(
            sum(r["critical_path_ms"] for r in summary_rows),
            45.0,
            places=2,
            msg=f"summary_rows for {eof_args} = {summary_rows}",
        )
        for row in summary_rows:
            self.assertEqual(
                row["unattributed_sleep_ms"],
                0.0,
                msg=f"unexpected sleep in row={row} for {eof_args}",
            )
        by_slice = {
            (r["thread_name"], r["slice_name"]): r["critical_path_ms"]
            for r in slice_rows
        }
        self.assertAlmostEqual(
            by_slice[("eof_target", "UnclosedTail")],
            13.0,
            places=2,
            msg=f"by_slice for {eof_args} = {by_slice}",
        )
        timer_handoffs = [
            r["handoff_from"]
            for r in timeline_rows
            if r["thread_name"] == "timer_worker" and r["handoff_from"]
        ]
        self.assertEqual(
            timer_handoffs,
            ["boot_proc:eof_target (blocked wait)"],
            msg=f"timer_worker handoffs in timeline={timeline_rows}",
        )

        # 3. Sleeping start and end timestamps resolve cleanly across wakeups (both s_next + e_sleep_wake and e_prev).
        for sleep_ep_args in (
            [
                "--start-tid",
                "503",
                "--start-ts",
                "25000000",
                "--end-tid",
                "502",
                "--end-ts",
                "35000000",
            ],
            [
                "--start-tid",
                "501",
                "--start-ts",
                "12000000",
                "--end-tid",
                "502",
                "--end-ts",
                "35000000",
            ],
        ):
            sleep_ep_res = plugin.analyze(sleep_ep_args, trace_path)
            for sec in sleep_ep_res:
                self.assertIsNone(
                    sec.error,
                    msg=f"section {sec.name!r} errored for {sleep_ep_args}: {sec.error}",
                )

    def test_reverse_time_flow_on_long_running_span_rejected(self) -> None:
        """Verifies Tier-2 backward search rejects non-monotonic forward time jumps on long spans."""
        ms = 1_000_000
        b = fxt.Builder()
        b.add_process(100, "proc")
        b.add_thread(100, 101, "late_sender")
        b.add_thread(100, 202, "event_loop")
        b.add_thread(100, 301, "early_target")
        idle_tid = 9999

        # event_loop (202) runs continuously [10..90 ms] on CPU 0.
        b.add_thread_wakeup(9 * ms, 0, 202)
        b.add_context_switch(
            10 * ms,
            0,
            THREAD_BLOCKED,
            idle_tid,
            202,
            outgoing_priority=IDLE_WEIGHT,
        )
        # late_sender (101) runs [12..75 ms] on CPU 1 with LateSenderSlice [12..71 ms].
        b.add_thread_wakeup(11 * ms, 1, 101)
        b.add_context_switch(
            12 * ms,
            1,
            THREAD_BLOCKED,
            idle_tid,
            101,
            outgoing_priority=IDLE_WEIGHT,
        )
        b.add_duration_begin("ipc", "LateSenderSlice", 12 * ms, 100, 101)

        # At 20 ms, event_loop wakes early_target (301), which runs EarlyTargetSlice [21..80 ms].
        b.add_thread_wakeup(20 * ms, 2, 301, waker_tid=202)
        b.add_context_switch(
            21 * ms,
            2,
            THREAD_BLOCKED,
            idle_tid,
            301,
            outgoing_priority=IDLE_WEIGHT,
        )
        b.add_duration_begin("work", "EarlyTargetSlice", 21 * ms, 100, 301)

        # Much later at 70 ms, late_sender emits FlowBegin(55), received by event_loop at 72 ms!
        b.add_flow_begin("ipc", "late_flow", 70 * ms, 100, 101, flow_id=55)
        b.add_duration_end("ipc", "LateSenderSlice", 71 * ms, 100, 101)
        b.add_duration_begin("ipc", "HandleLateFlow", 72 * ms, 100, 202)
        b.add_flow_end("ipc", "late_flow", 72 * ms, 100, 202, flow_id=55)
        b.add_duration_end("ipc", "HandleLateFlow", 78 * ms, 100, 202)
        b.add_duration_end("work", "EarlyTargetSlice", 80 * ms, 100, 301)

        trace_path = _write_temp_trace(b)
        self.addCleanup(self._remove_file_if_exists, trace_path)

        args = [
            "--start-slice",
            "LateSenderSlice",
            "--start-edge",
            "start",
            "--end-slice",
            "EarlyTargetSlice",
            "--end-edge",
            "end",
        ]
        sections = CriticalPathPlugin().analyze(args, trace_path)
        for sec in sections:
            assert sec.error is not None
            self.assertIn(
                "No causal scheduler wakeup or trace flow path connects",
                sec.error,
                msg=f"reverse-time flow for {args} should be rejected, got: {sec.error}",
            )

    def test_compute_critical_path_without_slice_table(self) -> None:
        """Verifies `compute_critical_path` handles traces lacking `slice` (`has_slices=False`)."""
        trace_path = self._build_wakeup_and_flow_trace()
        with PerfettoTraceProcessor(trace_path) as tp:
            db_objects = tp.get_tables() - {"slice"}
            rows = tp.run_query(
                "SELECT utid FROM thread WHERE tid = 101 LIMIT 1;"
            )
            utid = int(rows[0]["utid"])
            seg = SpineSegment(
                group_id=0,
                seg_index=0,
                utid=utid,
                ts=10_000_000,
                dur=40_000_000,
            )
            bd = compute_critical_path(tp, db_objects, [seg])
            self.assertIn(
                0,
                bd,
                msg=f"compute_critical_path(db_objects without 'slice', {[seg]}) returned {bd}",
            )
            self.assertEqual(
                bd[0].total_dur_ns,
                40_000_000,
                msg=f"total_dur_ns={bd[0].total_dur_ns}, want 40_000_000",
            )
            self.assertTrue(
                all(
                    s["slice_name"] == UNINSTRUMENTED_SLICE
                    for s in bd[0].top_slices
                ),
                msg=f"top_slices={bd[0].top_slices}",
            )

    def test_build_pa_nodes_preserves_ascending_id_order_with_bootstrap_threads(
        self,
    ) -> None:
        """Verifies `_pa_nodes` keeps `id` physically sorted when `bootstrap_utids` is non-empty."""
        ms = 1_000_000
        b = fxt.Builder()
        b.add_process(500, "boot_proc")
        b.add_thread(500, 501, "boot_waker")
        b.add_thread(500, 502, "woken_worker")
        idle_tid = 9999

        # `boot_waker` starts Running at 5 ms with no prior wakeup (`bootstrap_utids` row,
        # early `thread_state.id`), then wakes `woken_worker` at 20 ms (`_wakeup_graph` row,
        # larger `thread_state.id`).
        b.add_context_switch(
            5 * ms,
            0,
            THREAD_BLOCKED,
            idle_tid,
            501,
            outgoing_priority=IDLE_WEIGHT,
        )
        b.add_context_switch(
            8 * ms,
            1,
            THREAD_BLOCKED,
            idle_tid,
            502,
            outgoing_priority=IDLE_WEIGHT,
        )
        b.add_context_switch(
            10 * ms,
            1,
            THREAD_BLOCKED,
            502,
            idle_tid,
            incoming_priority=IDLE_WEIGHT,
        )
        b.add_thread_wakeup(20 * ms, 1, 502, waker_tid=501)
        b.add_context_switch(
            21 * ms,
            1,
            THREAD_BLOCKED,
            idle_tid,
            502,
            outgoing_priority=IDLE_WEIGHT,
        )
        b.add_context_switch(
            30 * ms,
            1,
            THREAD_BLOCKED,
            502,
            idle_tid,
            incoming_priority=IDLE_WEIGHT,
        )

        trace_path = _write_temp_trace(b)
        self.addCleanup(self._remove_file_if_exists, trace_path)

        with PerfettoTraceProcessor(trace_path) as tp:
            ddl = _build_pa_nodes_sql([(5 * ms, 30 * ms)], 30 * ms)
            rows = tp.run_query(f"{ddl}\nSELECT id FROM _pa_nodes;")
            node_ids = [int(r["id"]) for r in rows]
            self.assertGreaterEqual(
                len(node_ids),
                2,
                msg=f"_build_pa_nodes_sql([(5ms, 30ms)], 30ms) produced {node_ids}",
            )
            self.assertEqual(
                node_ids,
                sorted(node_ids),
                msg=(
                    f"_pa_nodes physical `id` column is unsorted ({node_ids}); "
                    "expected ascending order for Perfetto binary search"
                ),
            )


if __name__ == "__main__":
    unittest.main()
