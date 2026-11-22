#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for scenic.py."""

import json
import tempfile
import typing
import unittest
from unittest import mock

import gfx_fxt_test_utils
import test_utils
import trace_processing.metrics.scenic as scenic_metrics
from parameterized import param, parameterized
from reporting import metrics
from tp_shell import PerfettoTraceProcessor
from trace_processing import trace_model, trace_utils
from trace_writing import fxt

_U = metrics.Unit
_TCR = metrics.TestCaseResult

_SCENIC_PID = gfx_fxt_test_utils.SCENIC_PID
_SCENIC_TID = gfx_fxt_test_utils.SCENIC_TID
_DISPLAY_PID = gfx_fxt_test_utils.DISPLAY_PID
_DISPLAY_TID = gfx_fxt_test_utils.DISPLAY_TID
_OTHER_PID = gfx_fxt_test_utils.OTHER_PID


def _add_start_event(
    builder: fxt.Builder,
    ts_ns: int,
    frame_number: int | None,
    dur_ns: int = 10_000,
) -> None:
    args = {"frame_number": frame_number} if frame_number is not None else {}
    builder.add_duration_complete(
        "gfx",
        "ApplyScheduledSessionUpdates",
        ts_ns,
        dur_ns,
        _SCENIC_PID,
        _SCENIC_TID,
        args=args,
    )


def _add_scenic_frame(
    builder: fxt.Builder,
    start_ts_ns: int,
    render_ts_ns: int,
    vsync_ts_ns: int,
    start_flow_id: int,
    vsync_flow_id: int,
    *,
    frame_number: int = 1,
    start_dur_ns: int = 10_000,
    render_dur_ns: int = 20_000,
    vsync_dur_ns: int = 10_000,
    render_tid: int = _SCENIC_TID,
    vsync_tid: int = _DISPLAY_TID,
) -> None:
    _add_start_event(
        builder, start_ts_ns, frame_number=frame_number, dur_ns=start_dur_ns
    )
    gfx_fxt_test_utils.add_render_frame(
        builder, render_ts_ns, dur_ns=render_dur_ns, tid=render_tid
    )
    gfx_fxt_test_utils.add_vsync(
        builder, vsync_ts_ns, dur_ns=vsync_dur_ns, tid=vsync_tid
    )
    gfx_fxt_test_utils.add_flow(
        builder,
        "gfx",
        f"StartToRender{start_flow_id}",
        start_flow_id,
        _SCENIC_PID,
        _SCENIC_TID,
        start_ts_ns + 2_000,
        _SCENIC_PID,
        render_tid,
        render_ts_ns + 2_000,
    )
    gfx_fxt_test_utils.add_flow(
        builder,
        "gfx",
        f"RenderToVsync{vsync_flow_id}",
        vsync_flow_id,
        _SCENIC_PID,
        render_tid,
        render_ts_ns + 5_000,
        _DISPLAY_PID,
        vsync_tid,
        vsync_ts_ns + 1_000,
    )


def _process_and_inspect_fxt(
    builder: fxt.Builder,
) -> tuple[
    list[metrics.TestCaseResult],
    list[dict[str, typing.Any]],
    list[dict[str, typing.Any]],
]:
    """Runs ScenicMetricsProcessor unmocked on an FXT trace and inspects start/frame queries."""
    processor = scenic_metrics.ScenicMetricsProcessor(
        aggregates_only=False, include_render_total=True
    )
    with tempfile.NamedTemporaryFile(suffix=".fxt") as tmp:
        builder.write_to_file(tmp.name)
        with PerfettoTraceProcessor(trace_path=tmp.name) as tp:
            start_rows = tp.run_query(scenic_metrics._START_EVENTS_QUERY)
            frame_rows = tp.run_query(scenic_metrics._RENDER_VSYNC_QUERY)
        results = list(processor.process_metrics_with_fxt(tmp.name))
    return results, start_rows, frame_rows


class ScenicMetricsProcessorTest(unittest.TestCase):
    """Tests for SQL-based ScenicMetricsProcessor."""

    @parameterized.expand(
        [
            param(
                "time_series",
                aggregates_only=False,
                include_render_total=True,
                expected_results=[
                    _TCR(
                        label="RenderCpu",
                        unit=_U.milliseconds,
                        values=[0.09, 0.08],
                    ),
                    _TCR(
                        label="RenderTotal",
                        unit=_U.milliseconds,
                        values=[0.11, 0.112],
                    ),
                ],
            ),
            param(
                "aggregates",
                aggregates_only=True,
                include_render_total=True,
                expected_results=(
                    trace_utils.standard_metrics_set(
                        [0.09, 0.08],
                        "RenderCpu",
                        _U.milliseconds,
                    )
                    + trace_utils.standard_metrics_set(
                        [0.11, 0.112],
                        "RenderTotal",
                        _U.milliseconds,
                    )
                ),
            ),
            param(
                "no_render_total",
                aggregates_only=False,
                include_render_total=False,
                expected_results=[
                    _TCR(
                        label="RenderCpu",
                        unit=_U.milliseconds,
                        values=[0.09, 0.08],
                    ),
                ],
            ),
        ]
    )
    def test_process_metrics_with_fxt(
        self,
        _: str,
        aggregates_only: bool,
        include_render_total: bool,
        expected_results: list[_TCR],
    ) -> None:
        """Verifies FXT SQL scenic metrics match legacy expectations."""
        processor = scenic_metrics.ScenicMetricsProcessor(
            aggregates_only=aggregates_only,
            include_render_total=include_render_total,
        )
        fxt_path = test_utils.runtime_dep_path("scenic_metric.fxt")
        actual_results = list(processor.process_metrics_with_fxt(fxt_path))

        docs = processor.describe(actual_results)
        self.assertEqual(docs["classname"], processor.__class__.__name__)
        self.assertNotEqual(docs["doc"], "")
        self.assertNotEqual(docs["code_path"], "")

        self.maxDiff = 10000
        self.assertSequenceEqual(actual_results, expected_results)

    def test_empty_scenic_trace(self) -> None:
        """Returns [] when the FXT trace contains no ApplyScheduledSessionUpdates."""
        processor = scenic_metrics.ScenicMetricsProcessor(aggregates_only=False)
        fxt_path = test_utils.runtime_dep_path("cpu_metric.fxt")
        self.assertSequenceEqual(
            list(processor.process_metrics_with_fxt(fxt_path)), []
        )

    def test_legacy_model_methods_return_empty(self) -> None:
        """Legacy model-based hooks return empty sets/list while keeping signatures."""
        processor = scenic_metrics.ScenicMetricsProcessor()
        self.assertEqual(processor.event_patterns, set())
        self.assertEqual(processor.category_names, set())
        self.assertSequenceEqual(
            list(processor.process_metrics(trace_model.Model([], {}))), []
        )

    def test_chrome_json_args_frame_number_fallback(self) -> None:
        """Verifies Chrome-JSON args.frame_number fallback produces RenderCpu and RenderTotal."""
        processor = scenic_metrics.ScenicMetricsProcessor(
            aggregates_only=False, include_render_total=True
        )
        # Chrome-JSON timestamps and durations are in microseconds.
        trace_payload = {
            "displayTimeUnit": "ns",
            "traceEvents": [
                {
                    "cat": "gfx",
                    "name": "Init",
                    "ph": "X",
                    "pid": 2,
                    "tid": 2,
                    "ts": 50,
                    "dur": 5,
                },
                {
                    "cat": "gfx",
                    "name": "ApplyScheduledSessionUpdates",
                    "ph": "X",
                    "pid": 1,
                    "tid": 1,
                    "ts": 100,
                    "dur": 10,
                    "args": {"frame_number": 1},
                },
                {
                    "cat": "gfx",
                    "name": "RenderFrame",
                    "ph": "X",
                    "pid": 1,
                    "tid": 1,
                    "ts": 180,
                    "dur": 20,
                },
                {
                    "cat": "gfx",
                    "name": "Flatland::DisplayCompositor::OnVsync",
                    "ph": "X",
                    "pid": 2,
                    "tid": 2,
                    "ts": 250,
                    "dur": 10,
                },
                {
                    "cat": "gfx",
                    "name": "StartToRender",
                    "ph": "s",
                    "id": 1,
                    "pid": 1,
                    "tid": 1,
                    "ts": 105,
                },
                {
                    "cat": "gfx",
                    "name": "StartToRender",
                    "ph": "f",
                    "bp": "e",
                    "id": 1,
                    "pid": 1,
                    "tid": 1,
                    "ts": 185,
                },
                {
                    "cat": "gfx",
                    "name": "RenderToVsync",
                    "ph": "s",
                    "id": 2,
                    "pid": 1,
                    "tid": 1,
                    "ts": 190,
                },
                {
                    "cat": "gfx",
                    "name": "RenderToVsync",
                    "ph": "f",
                    "bp": "e",
                    "id": 2,
                    "pid": 2,
                    "tid": 2,
                    "ts": 255,
                },
            ],
            "systemTraceEvents": (
                "# tracer: nop\n"
                "other-99    [000] d..2 0.000120: sched_switch: "
                "prev_comm=other prev_pid=99 prev_prio=120 prev_state=S ==> "
                "next_comm=scenic-main next_pid=1 next_prio=120\n"
                "scenic-main-1     [000] d..2 0.000150: sched_switch: "
                "prev_comm=scenic-main prev_pid=1 prev_prio=120 prev_state=S ==> "
                "next_comm=other next_pid=99 next_prio=120\n"
            ),
        }
        with tempfile.NamedTemporaryFile(mode="w", suffix=".json") as tmp:
            json.dump(trace_payload, tmp)
            tmp.flush()
            results = list(processor.process_metrics_with_fxt(tmp.name))

        self.assertSequenceEqual(
            results,
            [
                _TCR(label="RenderCpu", unit=_U.milliseconds, values=[0.03]),
                _TCR(label="RenderTotal", unit=_U.milliseconds, values=[0.15]),
            ],
        )

    def test_missing_frame_number_raises_value_error(self) -> None:
        """Raises ValueError if a kept ApplyScheduledSessionUpdates lacks frame_number."""
        processor = scenic_metrics.ScenicMetricsProcessor(aggregates_only=False)
        builder = gfx_fxt_test_utils.new_gfx_builder()
        _add_start_event(builder, 100_000, frame_number=None)
        with self.assertRaises(ValueError):
            gfx_fxt_test_utils.process_fxt(builder, processor)

    def test_missing_sched_records_raises_value_error(self) -> None:
        """Raises ValueError when frames exist but sched table is empty."""
        processor = scenic_metrics.ScenicMetricsProcessor(aggregates_only=False)
        builder = gfx_fxt_test_utils.new_gfx_builder()
        gfx_fxt_test_utils.add_vsync(builder, 50_000)
        _add_scenic_frame(
            builder,
            start_ts_ns=100_000,
            render_ts_ns=120_000,
            vsync_ts_ns=200_000,
            start_flow_id=1,
            vsync_flow_id=2,
            render_dur_ns=10_000,
        )
        with self.assertRaises(ValueError):
            gfx_fxt_test_utils.process_fxt(builder, processor)

    def test_squashed_frame_deduplication_queries(self) -> None:
        """Deduplicates squashed frames by MAX(ts) per frame_number in SQL."""
        builder = gfx_fxt_test_utils.new_gfx_builder()
        # Squashed start event (100 us) is superseded by second start (150 us) for frame 7.
        _add_start_event(builder, 100_000, frame_number=7, dur_ns=5_000)
        _add_scenic_frame(
            builder,
            start_ts_ns=150_000,
            render_ts_ns=160_000,
            vsync_ts_ns=250_000,
            start_flow_id=1,
            vsync_flow_id=2,
            frame_number=7,
            start_dur_ns=5_000,
            render_dur_ns=30_000,
            vsync_dur_ns=5_000,
        )

        with tempfile.NamedTemporaryFile(suffix=".fxt") as tmp:
            builder.write_to_file(tmp.name)
            with PerfettoTraceProcessor(trace_path=tmp.name) as tp:
                start_rows = tp.run_query(scenic_metrics._START_EVENTS_QUERY)
                frame_rows = tp.run_query(scenic_metrics._RENDER_VSYNC_QUERY)

        self.assertIsNone(start_rows[0]["missing_frame_number_ts"])
        self.assertEqual(int(start_rows[0]["start_count"]), 1)
        self.assertEqual(len(frame_rows), 1)
        self.assertEqual(int(frame_rows[0]["start_ts"]), 150_000)
        self.assertEqual(int(frame_rows[0]["render_end_ts"]), 190_000)
        self.assertEqual(int(frame_rows[0]["vsync_ts"]), 250_000)

    def test_sched_running_durations_and_open_ended_switch(self) -> None:
        """Computes RenderCpu from FXT context switches including preemption and dur=-1."""
        processor = scenic_metrics.ScenicMetricsProcessor(
            aggregates_only=False, include_render_total=True
        )
        builder = gfx_fxt_test_utils.new_gfx_builder()
        builder.add_process(_OTHER_PID, "other")
        builder.add_thread(_OTHER_PID, 99, "other-main")
        builder.add_duration_complete(
            "gfx", "Init", 50_000, 5_000, _DISPLAY_PID, _DISPLAY_TID
        )

        # Frame 1: [100_000, 200_000], vsync at 250_000 -> RenderTotal = 0.15 ms.
        _add_scenic_frame(
            builder,
            start_ts_ns=100_000,
            render_ts_ns=180_000,
            vsync_ts_ns=250_000,
            start_flow_id=1,
            vsync_flow_id=2,
            frame_number=1,
        )
        # Preempted in window 1: [120_000, 150_000) = 30_000 ns = 0.03 ms.
        builder.add_context_switch(120_000, 0, 2, 99, _SCENIC_TID)
        builder.add_context_switch(150_000, 0, 2, _SCENIC_TID, 99)

        # Frame 2: [300_000, 400_000], vsync at 460_000 -> RenderTotal = 0.16 ms.
        _add_scenic_frame(
            builder,
            start_ts_ns=300_000,
            render_ts_ns=380_000,
            vsync_ts_ns=460_000,
            start_flow_id=3,
            vsync_flow_id=4,
            frame_number=2,
        )
        # Open-ended switch in window 2: [320_000, 400_000) = 80_000 ns = 0.08 ms.
        builder.add_context_switch(320_000, 0, 2, 99, _SCENIC_TID)

        results = gfx_fxt_test_utils.process_fxt(builder, processor)

        self.assertSequenceEqual(
            results,
            [
                _TCR(
                    label="RenderCpu",
                    unit=_U.milliseconds,
                    values=[0.03, 0.08],
                ),
                _TCR(
                    label="RenderTotal",
                    unit=_U.milliseconds,
                    values=[0.15, 0.16],
                ),
            ],
        )

    def test_sched_present_for_other_threads_returns_zero_cpu(self) -> None:
        """Returns 0.0 RenderCpu when sched has records for other threads but not render_utid."""
        processor = scenic_metrics.ScenicMetricsProcessor(
            aggregates_only=False, include_render_total=False
        )
        builder = gfx_fxt_test_utils.new_gfx_builder()
        builder.add_process(_OTHER_PID, "other")
        builder.add_thread(_OTHER_PID, 99, "other-a")
        builder.add_thread(_OTHER_PID, 100, "other-b")
        builder.add_duration_complete(
            "gfx", "Init", 50_000, 5_000, _DISPLAY_PID, _DISPLAY_TID
        )

        _add_scenic_frame(
            builder,
            start_ts_ns=100_000,
            render_ts_ns=180_000,
            vsync_ts_ns=250_000,
            start_flow_id=1,
            vsync_flow_id=2,
            frame_number=1,
        )
        # Context switches exist in sched, but only for unrelated threads.
        builder.add_context_switch(120_000, 0, 2, 99, 100)
        builder.add_context_switch(160_000, 0, 2, 100, 99)

        results = gfx_fxt_test_utils.process_fxt(builder, processor)

        self.assertSequenceEqual(
            results,
            [_TCR(label="RenderCpu", unit=_U.milliseconds, values=[0.0])],
        )

    def test_start_events_present_with_no_completed_frames(self) -> None:
        """Returns [] with warning when start_count > 0 but frame_rows is empty."""
        processor = scenic_metrics.ScenicMetricsProcessor(aggregates_only=False)
        builder = gfx_fxt_test_utils.new_gfx_builder()
        _add_start_event(builder, 100_000, frame_number=1)
        gfx_fxt_test_utils.add_render_frame(builder, 120_000)

        with self.assertLogs(scenic_metrics._LOGGER, level="WARNING"):
            results = gfx_fxt_test_utils.process_fxt(builder, processor)

        self.assertSequenceEqual(results, [])

    def test_sched_multiple_utids_and_zero_dur(self) -> None:
        """Handles multiple render_utids and zero-duration sched rows in FXT traces."""
        processor = scenic_metrics.ScenicMetricsProcessor(
            aggregates_only=False, include_render_total=True
        )
        render_tid2 = 6
        builder = gfx_fxt_test_utils.new_gfx_builder()
        builder.add_thread(_SCENIC_PID, render_tid2, "scenic-render-2")
        builder.add_process(_OTHER_PID, "other")
        builder.add_thread(_OTHER_PID, 99, "other-main")
        builder.add_duration_complete(
            "gfx", "Init", 50_000, 5_000, _DISPLAY_PID, _DISPLAY_TID
        )

        # Frame 1 on render_tid2: [100_000, 200_000], vsync 250_000.
        _add_scenic_frame(
            builder,
            start_ts_ns=100_000,
            render_ts_ns=180_000,
            vsync_ts_ns=250_000,
            start_flow_id=1,
            vsync_flow_id=2,
            frame_number=1,
            render_tid=render_tid2,
        )
        # Zero-duration switch-in/out at 110_000 plus [120_000, 150_000) = 0.03 ms.
        builder.add_context_switch(110_000, 0, 2, 99, render_tid2)
        builder.add_context_switch(110_000, 0, 2, render_tid2, 99)
        builder.add_context_switch(120_000, 0, 2, 99, render_tid2)
        builder.add_context_switch(150_000, 0, 2, render_tid2, 99)

        # Frame 2 on _SCENIC_TID: [300_000, 400_000], vsync 450_000.
        _add_scenic_frame(
            builder,
            start_ts_ns=300_000,
            render_ts_ns=380_000,
            vsync_ts_ns=450_000,
            start_flow_id=3,
            vsync_flow_id=4,
            frame_number=2,
        )
        builder.add_context_switch(310_000, 0, 2, 99, _SCENIC_TID)
        builder.add_context_switch(350_000, 0, 2, _SCENIC_TID, 99)

        results = gfx_fxt_test_utils.process_fxt(builder, processor)

        self.assertSequenceEqual(
            results,
            [
                _TCR(
                    label="RenderCpu",
                    unit=_U.milliseconds,
                    values=[0.03, 0.04],
                ),
                _TCR(
                    label="RenderTotal",
                    unit=_U.milliseconds,
                    values=[0.15, 0.15],
                ),
            ],
        )

    @mock.patch.object(
        scenic_metrics,
        "PerfettoTraceProcessor",
        autospec=True,
        spec_set=True,
    )
    def test_sched_partial_render_utid_none_skips_sample(
        self, mock_cls: mock.Mock
    ) -> None:
        """Logs warning and skips RenderCpu sample when render_utid is None."""
        processor = scenic_metrics.ScenicMetricsProcessor(
            aggregates_only=False, include_render_total=True
        )
        mock_tp = mock_cls.return_value.__enter__.return_value
        mock_tp.run_query.side_effect = [
            [{"missing_frame_number_ts": None, "start_count": 3}],
            [
                {
                    "start_ts": 100_000,
                    "render_end_ts": 200_000,
                    "render_utid": 6,
                    "vsync_ts": 250_000,
                },
                {
                    "start_ts": 300_000,
                    "render_end_ts": 400_000,
                    "render_utid": None,
                    "vsync_ts": 450_000,
                },
                {
                    "start_ts": 500_000,
                    "render_end_ts": 600_000,
                    "render_utid": 5,
                    "vsync_ts": 650_000,
                },
            ],
        ]
        mock_tp.query_rows.return_value = (
            ("utid", "ts", "dur"),
            iter([(6, 120_000, 30_000), (5, 510_000, 40_000)]),
        )

        with self.assertLogs(scenic_metrics._LOGGER, level="WARNING") as logs:
            results = list(processor.process_metrics_with_fxt("trace.fxt"))

        self.assertTrue(
            any("render_utid is None" in msg for msg in logs.output)
        )
        self.assertSequenceEqual(
            results,
            [
                _TCR(
                    label="RenderCpu",
                    unit=_U.milliseconds,
                    values=[0.03, 0.04],
                ),
                _TCR(
                    label="RenderTotal",
                    unit=_U.milliseconds,
                    values=[0.15, 0.15, 0.15],
                ),
            ],
        )

    @mock.patch.object(
        scenic_metrics,
        "PerfettoTraceProcessor",
        autospec=True,
        spec_set=True,
    )
    def test_sched_all_render_utids_none(self, mock_cls: mock.Mock) -> None:
        """Skips sched check and emits RenderTotal when all render_utids are None."""
        processor = scenic_metrics.ScenicMetricsProcessor(
            aggregates_only=False, include_render_total=True
        )
        mock_tp = mock_cls.return_value.__enter__.return_value
        mock_tp.run_query.side_effect = [
            [{"missing_frame_number_ts": None, "start_count": 1}],
            [
                {
                    "start_ts": 100_000,
                    "render_end_ts": 200_000,
                    "render_utid": None,
                    "vsync_ts": 250_000,
                },
            ],
        ]

        with self.assertLogs(scenic_metrics._LOGGER, level="WARNING") as logs:
            results = list(processor.process_metrics_with_fxt("trace.fxt"))

        self.assertEqual(mock_tp.run_query.call_count, 2)
        mock_tp.query_rows.assert_not_called()
        self.assertTrue(
            any("render_utid is None" in msg for msg in logs.output)
        )
        self.assertSequenceEqual(
            results,
            [_TCR(label="RenderTotal", unit=_U.milliseconds, values=[0.15])],
        )

    def test_sched_open_ended_switch_mid_trace_and_past_window_end(
        self,
    ) -> None:
        """Only extends dur=-1 when it is the final sched row for that utid and ts < last_window_end."""
        processor = scenic_metrics.ScenicMetricsProcessor(
            aggregates_only=False, include_render_total=False
        )

        # Case 1: mid-trace dur=-1 (CPU 0 never switched out) followed by another
        # sched row for _SCENIC_TID (CPU 1) must NOT extend to 400_000; only the
        # final dur=-1 with ts < last_window_end (350_000) is extended to 400_000.
        builder1 = gfx_fxt_test_utils.new_gfx_builder()
        builder1.add_process(_OTHER_PID, "other")
        builder1.add_thread(_OTHER_PID, 99, "other-main")
        builder1.add_duration_complete(
            "gfx", "Init", 50_000, 5_000, _DISPLAY_PID, _DISPLAY_TID
        )
        _add_scenic_frame(
            builder1,
            start_ts_ns=100_000,
            render_ts_ns=180_000,
            vsync_ts_ns=250_000,
            start_flow_id=1,
            vsync_flow_id=2,
            frame_number=1,
        )
        _add_scenic_frame(
            builder1,
            start_ts_ns=300_000,
            render_ts_ns=380_000,
            vsync_ts_ns=450_000,
            start_flow_id=3,
            vsync_flow_id=4,
            frame_number=2,
        )
        builder1.add_context_switch(110_000, 0, 2, 99, _SCENIC_TID)
        builder1.add_context_switch(150_000, 1, 2, 99, _SCENIC_TID)
        builder1.add_context_switch(170_000, 1, 2, _SCENIC_TID, 99)
        builder1.add_context_switch(350_000, 1, 2, 99, _SCENIC_TID)

        self.assertSequenceEqual(
            gfx_fxt_test_utils.process_fxt(builder1, processor),
            [
                _TCR(
                    label="RenderCpu", unit=_U.milliseconds, values=[0.02, 0.05]
                )
            ],
        )

        # Case 2: final dur=-1 with ts >= last_window_end contributes 0.
        render_tid2 = 6
        builder2 = gfx_fxt_test_utils.new_gfx_builder()
        builder2.add_thread(_SCENIC_PID, render_tid2, "scenic-render-2")
        builder2.add_process(_OTHER_PID, "other")
        builder2.add_thread(_OTHER_PID, 99, "other-main")
        builder2.add_duration_complete(
            "gfx", "Init", 50_000, 5_000, _DISPLAY_PID, _DISPLAY_TID
        )
        _add_scenic_frame(
            builder2,
            start_ts_ns=100_000,
            render_ts_ns=180_000,
            vsync_ts_ns=250_000,
            start_flow_id=1,
            vsync_flow_id=2,
            frame_number=1,
        )
        _add_scenic_frame(
            builder2,
            start_ts_ns=300_000,
            render_ts_ns=380_000,
            vsync_ts_ns=450_000,
            start_flow_id=3,
            vsync_flow_id=4,
            frame_number=2,
            render_tid=render_tid2,
        )
        builder2.add_context_switch(120_000, 0, 2, 99, _SCENIC_TID)
        builder2.add_context_switch(145_000, 0, 2, _SCENIC_TID, 99)
        builder2.add_context_switch(200_000, 0, 2, 99, _SCENIC_TID)
        builder2.add_context_switch(450_000, 1, 2, 99, render_tid2)

        self.assertSequenceEqual(
            gfx_fxt_test_utils.process_fxt(builder2, processor),
            [
                _TCR(
                    label="RenderCpu", unit=_U.milliseconds, values=[0.025, 0.0]
                )
            ],
        )

    def test_compute_window_overlaps(self) -> None:
        """Directly tests _compute_window_overlaps on boundary-straddling and merged segments."""
        self.assertSequenceEqual(
            scenic_metrics._compute_window_overlaps([(0, 100)], []), []
        )
        self.assertSequenceEqual(
            scenic_metrics._compute_window_overlaps(
                [], [(100, 200), (300, 400)]
            ),
            [0, 0],
        )

        windows = [(100, 200), (250, 350), (400, 500)]
        segments = [
            (50, 100),  # touches left boundary of window 0
            (90, 130),  # straddles left boundary -> 30 in window 0
            (120, 150),  # overlaps (90, 130) -> merged (90, 150) -> 50 total
            (180, 270),  # straddles window 0 right (20) & window 1 left (20)
            (290, 420),  # straddles window 1 right (60) & window 2 left (20)
            (500, 600),  # touches right boundary of window 2
        ]
        self.assertSequenceEqual(
            scenic_metrics._compute_window_overlaps(segments, windows),
            [70, 80, 20],
        )

        # Single segment spanning completely across multiple windows.
        self.assertSequenceEqual(
            scenic_metrics._compute_window_overlaps(
                [(200, 600)],
                [(100, 150), (250, 350), (400, 450), (700, 800)],
            ),
            [0, 100, 50, 0],
        )

        # Overlapping pipelined windows (w_start[i + 1] < w_end[i]).
        self.assertSequenceEqual(
            scenic_metrics._compute_window_overlaps(
                [(80, 120), (160, 200), (210, 260), (300, 330)],
                [(100, 220), (150, 280), (180, 350)],
            ),
            [70, 90, 100],
        )

        # Unsorted input segments are sorted before merging and sweeping.
        self.assertSequenceEqual(
            scenic_metrics._compute_window_overlaps(
                [(300, 350), (100, 140), (220, 260), (130, 180)],
                [(110, 170), (200, 330)],
            ),
            [60, 70],
        )

        # Completely contained segment preserves max(prev_end, end) = 200.
        self.assertSequenceEqual(
            scenic_metrics._compute_window_overlaps(
                [(100, 200), (120, 150)],
                [(100, 200), (160, 200)],
            ),
            [100, 40],
        )

    def test_start_and_render_duration_filters(self) -> None:
        """Excludes dur=0/dur=-1 ApplyScheduledSessionUpdates and RenderFrame slices."""
        builder = gfx_fxt_test_utils.new_gfx_builder()
        for tid in (2, 3, 11, 12, 13):
            builder.add_thread(_SCENIC_PID, tid, f"scenic-{tid}")
        builder.add_duration_complete(
            "gfx", "Init", 50_000, 5_000, _DISPLAY_PID, _DISPLAY_TID
        )

        # Instant and unterminated ApplyScheduledSessionUpdates without frame_number
        # must be ignored and must not trigger ValueError.
        builder.add_instant(
            "gfx", "ApplyScheduledSessionUpdates", 80_000, _SCENIC_PID, 11
        )
        builder.add_duration_begin(
            "gfx", "ApplyScheduledSessionUpdates", 85_000, _SCENIC_PID, 12
        )

        # Valid ApplyScheduledSessionUpdates for frame_number=1 (100 us);
        # instant with same frame_number=1 at 105 us must not stomp it.
        _add_start_event(builder, 100_000, frame_number=1)
        builder.add_instant(
            "gfx",
            "ApplyScheduledSessionUpdates",
            105_000,
            _SCENIC_PID,
            13,
            args={"frame_number": 1},
        )
        builder.add_flow_begin(
            "gfx", "ToInstantRender", 106_000, _SCENIC_PID, _SCENIC_TID, 1
        )
        builder.add_flow_begin(
            "gfx", "ToUnterminatedRender", 107_000, _SCENIC_PID, _SCENIC_TID, 2
        )

        # Instant RenderFrame (115 us) forwards flow to valid RenderFrame (120..140 us);
        # unterminated RenderFrame (118 us) is ignored.
        builder.add_instant("gfx", "RenderFrame", 115_000, _SCENIC_PID, 2)
        builder.add_flow_end(
            "gfx", "ToInstantRender", 115_000, _SCENIC_PID, 2, 1
        )
        builder.add_flow_begin(
            "gfx", "FromInstantRender", 115_000, _SCENIC_PID, 2, 3
        )
        builder.add_duration_begin(
            "gfx", "RenderFrame", 118_000, _SCENIC_PID, 3
        )
        builder.add_flow_end(
            "gfx", "ToUnterminatedRender", 119_000, _SCENIC_PID, 3, 2
        )

        gfx_fxt_test_utils.add_render_frame(builder, 120_000, dur_ns=20_000)
        builder.add_flow_end(
            "gfx", "FromInstantRender", 121_000, _SCENIC_PID, _SCENIC_TID, 3
        )
        gfx_fxt_test_utils.add_vsync(builder, 300_000)
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "ToVsync",
            4,
            _SCENIC_PID,
            _SCENIC_TID,
            125_000,
            _DISPLAY_PID,
            _DISPLAY_TID,
            305_000,
        )

        builder.add_process(_OTHER_PID, "other")
        builder.add_thread(_OTHER_PID, 99, "other-main")
        builder.add_context_switch(90_000, 0, 2, 99, _SCENIC_TID)
        builder.add_context_switch(450_000, 0, 2, _SCENIC_TID, 99)

        results, start_rows, frame_rows = _process_and_inspect_fxt(builder)
        self.assertIsNone(start_rows[0]["missing_frame_number_ts"])
        self.assertEqual(int(start_rows[0]["start_count"]), 1)
        self.assertEqual(len(frame_rows), 1)
        self.assertSequenceEqual(
            results,
            [
                _TCR(label="RenderCpu", unit=_U.milliseconds, values=[0.04]),
                _TCR(label="RenderTotal", unit=_U.milliseconds, values=[0.2]),
            ],
        )

    def test_timestamp_guards_before_start_and_render_ignored(self) -> None:
        """Enforces rf.ts >= se.ts and v.ts >= sr.render_ts including equality."""
        builder = gfx_fxt_test_utils.new_gfx_builder()
        for tid in (12, 13):
            builder.add_thread(_SCENIC_PID, tid, f"scenic-{tid}")
        for tid in (5, 6):
            builder.add_thread(_DISPLAY_PID, tid, f"display-{tid}")
        builder.add_process(_OTHER_PID, "other")
        builder.add_thread(_OTHER_PID, 99, "other-main")

        # Frame 1: SessionWork (60..200 us) encloses early RenderFrame (80 us < 100 us)
        # and valid RenderFrame (120..145 us). DisplayFrame (70..250 us) encloses
        # early OnVsync (90 us < 120 us) and valid OnVsync (200 us).
        builder.add_duration_complete(
            "gfx", "SessionWork", 60_000, 140_000, _SCENIC_PID, 12
        )
        builder.add_duration_complete(
            "gfx", "DisplayFrame", 70_000, 180_000, _DISPLAY_PID, 5
        )
        gfx_fxt_test_utils.add_render_frame(
            builder, 80_000, dur_ns=15_000, tid=12
        )
        gfx_fxt_test_utils.add_vsync(builder, 90_000, tid=6)
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "EarlyCommit",
            3,
            _DISPLAY_PID,
            5,
            75_000,
            _DISPLAY_PID,
            6,
            95_000,
        )
        _add_start_event(builder, 100_000, frame_number=1)
        builder.add_flow_begin(
            "gfx", "StartFlow", 105_000, _SCENIC_PID, _SCENIC_TID, 1
        )
        builder.add_instant("gfx", "ReceiveStart", 115_000, _SCENIC_PID, 12)
        builder.add_flow_end("gfx", "StartFlow", 115_000, _SCENIC_PID, 12, 1)
        gfx_fxt_test_utils.add_render_frame(
            builder, 120_000, dur_ns=25_000, tid=12
        )
        builder.add_flow_begin(
            "gfx", "ToDisplayFrame", 125_000, _SCENIC_PID, 12, 2
        )
        builder.add_instant("gfx", "ReceiveRender", 150_000, _DISPLAY_PID, 5)
        builder.add_flow_end(
            "gfx", "ToDisplayFrame", 150_000, _DISPLAY_PID, 5, 2
        )
        gfx_fxt_test_utils.add_vsync(builder, 200_000, tid=6)
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "LateCommit",
            4,
            _DISPLAY_PID,
            5,
            160_000,
            _DISPLAY_PID,
            6,
            205_000,
        )

        # Frame 2: exact equality boundaries rf.ts == se.ts == v.ts (300 us).
        _add_start_event(builder, 300_000, frame_number=2)
        gfx_fxt_test_utils.add_render_frame(
            builder, 300_000, dur_ns=20_000, tid=13
        )
        gfx_fxt_test_utils.add_vsync(builder, 300_000, tid=6)
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "EqStartFlow",
            5,
            _SCENIC_PID,
            _SCENIC_TID,
            300_000,
            _SCENIC_PID,
            13,
            301_000,
        )
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "EqVsyncFlow",
            6,
            _SCENIC_PID,
            13,
            302_000,
            _DISPLAY_PID,
            6,
            303_000,
        )

        builder.add_context_switch(90_000, 0, 2, 99, 12)
        builder.add_context_switch(200_000, 0, 2, 12, 99)
        builder.add_context_switch(290_000, 0, 2, 99, 13)
        builder.add_context_switch(350_000, 0, 2, 13, 99)

        results, _, frame_rows = _process_and_inspect_fxt(builder)
        self.assertEqual(len(frame_rows), 2)
        self.assertSequenceEqual(
            results,
            [
                _TCR(
                    label="RenderCpu",
                    unit=_U.milliseconds,
                    values=[0.045, 0.02],
                ),
                _TCR(
                    label="RenderTotal", unit=_U.milliseconds, values=[0.1, 0.0]
                ),
            ],
        )

    def test_common_start_filtering(self) -> None:
        """Filters pre-common-start ApplyScheduledSessionUpdates while ignoring unconnected upids."""
        builder = gfx_fxt_test_utils.new_gfx_builder()
        builder.add_process(_OTHER_PID, "unrelated")
        builder.add_thread(_OTHER_PID, 3, "unrelated-main")

        # Early frame (10 us) and missing-frame_number start (30 us) before Display
        # starts at 50 us are dropped. Frames at 50 us and 100 us are kept even
        # though unrelated pid=3 starts at 500 us.
        _add_scenic_frame(
            builder,
            start_ts_ns=10_000,
            render_ts_ns=20_000,
            vsync_ts_ns=50_000,
            start_flow_id=1,
            vsync_flow_id=2,
            frame_number=1,
            render_dur_ns=15_000,
        )
        _add_start_event(builder, 30_000, frame_number=None)
        _add_scenic_frame(
            builder,
            start_ts_ns=50_000,
            render_ts_ns=60_000,
            vsync_ts_ns=90_000,
            start_flow_id=3,
            vsync_flow_id=4,
            frame_number=2,
        )
        _add_scenic_frame(
            builder,
            start_ts_ns=100_000,
            render_ts_ns=110_000,
            vsync_ts_ns=150_000,
            start_flow_id=5,
            vsync_flow_id=6,
            frame_number=3,
            render_dur_ns=25_000,
        )
        builder.add_duration_complete(
            "gfx", "UnrelatedWork", 500_000, 10_000, _OTHER_PID, 3
        )
        builder.add_context_switch(40_000, 0, 2, 3, _SCENIC_TID)
        builder.add_context_switch(160_000, 0, 2, _SCENIC_TID, 3)

        results, start_rows, frame_rows = _process_and_inspect_fxt(builder)
        self.assertIsNone(start_rows[0]["missing_frame_number_ts"])
        self.assertEqual(int(start_rows[0]["start_count"]), 2)
        self.assertEqual(len(frame_rows), 2)
        self.assertSequenceEqual(
            results,
            [
                _TCR(
                    label="RenderCpu",
                    unit=_U.milliseconds,
                    values=[0.03, 0.035],
                ),
                _TCR(
                    label="RenderTotal",
                    unit=_U.milliseconds,
                    values=[0.04, 0.05],
                ),
            ],
        )

    def test_earliest_render_frame_selection(self) -> None:
        """Selects earliest RenderFrame by (rf.ts ASC, rf.id ASC) when multiple are reachable."""
        builder = gfx_fxt_test_utils.new_gfx_builder()
        builder.add_thread(_SCENIC_PID, 2, "scenic-2")
        builder.add_thread(_DISPLAY_PID, 3, "display-vsync")
        builder.add_thread(_DISPLAY_PID, 10, "display-init")

        builder.add_duration_complete(
            "gfx", "Init", 50_000, 5_000, _DISPLAY_PID, 10
        )

        # Frame 1 (100 us) reaches 150..190 us (emitted first -> smaller slice id)
        # and 120..140 us (earlier ts); must pick 120..140 us -> OnVsync 200 us.
        _add_start_event(builder, 100_000, frame_number=1)
        gfx_fxt_test_utils.add_render_frame(
            builder, 150_000, dur_ns=40_000, tid=2
        )
        gfx_fxt_test_utils.add_render_frame(builder, 120_000, dur_ns=20_000)
        gfx_fxt_test_utils.add_vsync(builder, 200_000, tid=3)
        gfx_fxt_test_utils.add_vsync(builder, 250_000, tid=3)
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "ToLaterRender",
            1,
            _SCENIC_PID,
            _SCENIC_TID,
            104_000,
            _SCENIC_PID,
            2,
            151_000,
        )
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "ToEarlierRender",
            2,
            _SCENIC_PID,
            _SCENIC_TID,
            105_000,
            _SCENIC_PID,
            _SCENIC_TID,
            121_000,
        )
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "LaterVsyncFlow",
            3,
            _SCENIC_PID,
            2,
            155_000,
            _DISPLAY_PID,
            3,
            251_000,
        )
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "EarlierVsyncFlow",
            4,
            _SCENIC_PID,
            _SCENIC_TID,
            125_000,
            _DISPLAY_PID,
            3,
            201_000,
        )

        # Frame 2 (300 us) reaches 320..335 us and 330..360 us -> picks 320..335 us -> 360 us.
        _add_start_event(builder, 300_000, frame_number=2)
        gfx_fxt_test_utils.add_render_frame(builder, 320_000, dur_ns=15_000)
        gfx_fxt_test_utils.add_render_frame(
            builder, 330_000, dur_ns=30_000, tid=2
        )
        gfx_fxt_test_utils.add_vsync(builder, 360_000, tid=3)
        gfx_fxt_test_utils.add_vsync(builder, 390_000, tid=3)
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "TieFlowA",
            5,
            _SCENIC_PID,
            _SCENIC_TID,
            304_000,
            _SCENIC_PID,
            _SCENIC_TID,
            321_000,
        )
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "TieFlowB",
            6,
            _SCENIC_PID,
            _SCENIC_TID,
            305_000,
            _SCENIC_PID,
            2,
            331_000,
        )
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "TieVsyncA",
            7,
            _SCENIC_PID,
            _SCENIC_TID,
            325_000,
            _DISPLAY_PID,
            3,
            361_000,
        )
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "TieVsyncB",
            8,
            _SCENIC_PID,
            2,
            335_000,
            _DISPLAY_PID,
            3,
            391_000,
        )

        builder.add_process(_OTHER_PID, "other")
        builder.add_thread(_OTHER_PID, 99, "other-main")
        builder.add_context_switch(90_000, 0, 2, 99, _SCENIC_TID)
        builder.add_context_switch(400_000, 0, 2, _SCENIC_TID, 99)

        results, _, frame_rows = _process_and_inspect_fxt(builder)
        self.assertEqual(len(frame_rows), 2)
        self.assertSequenceEqual(
            results,
            [
                _TCR(
                    label="RenderCpu",
                    unit=_U.milliseconds,
                    values=[0.04, 0.035],
                ),
                _TCR(
                    label="RenderTotal",
                    unit=_U.milliseconds,
                    values=[0.1, 0.06],
                ),
            ],
        )
