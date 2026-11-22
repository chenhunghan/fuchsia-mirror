#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for app_render.py."""

import unittest
from unittest import mock

import gfx_fxt_test_utils
import test_utils
import trace_processing.metrics.app_render as app_render_metrics
from parameterized import param, parameterized
from reporting import metrics
from tp_shell import PerfettoTraceProcessor
from trace_processing import trace_model, trace_utils
from trace_processing.metrics import _gfx_sql
from trace_writing import fxt

_U = metrics.Unit
_TCR = metrics.TestCaseResult

_SCENIC_PID = gfx_fxt_test_utils.SCENIC_PID
_SCENIC_TID = gfx_fxt_test_utils.SCENIC_TID
_DISPLAY_PID = gfx_fxt_test_utils.DISPLAY_PID
_DISPLAY_TID = gfx_fxt_test_utils.DISPLAY_TID
_APP_PID = gfx_fxt_test_utils.APP_PID
_APP_TID = gfx_fxt_test_utils.APP_TID


def _add_present(
    builder: fxt.Builder,
    app_name: str,
    ts_ns: int,
    dur_ns: int = 5_000,
    tid: int = _SCENIC_TID,
) -> None:
    builder.add_duration_complete(
        "gfx",
        f"Flatland::PerAppPresent[{app_name}]",
        ts_ns,
        dur_ns,
        _SCENIC_PID,
        tid,
    )


def _add_present_frame(
    builder: fxt.Builder,
    app_name: str,
    flow_begin_ts_ns: int,
    present_ts_ns: int,
    vsync_ts_ns: int,
    app_flow_id: int,
    vsync_flow_id: int,
    *,
    flow_begin_dur_ns: int = 2_000,
    present_dur_ns: int = 5_000,
    vsync_dur_ns: int = 10_000,
    app_tid: int = _APP_TID,
    scenic_tid: int = _SCENIC_TID,
    vsync_tid: int = _DISPLAY_TID,
) -> None:
    builder.add_duration_complete(
        "gfx",
        "FlatlandApp::PresentFlowBegin",
        flow_begin_ts_ns,
        flow_begin_dur_ns,
        _APP_PID,
        app_tid,
    )
    _add_present(
        builder,
        app_name,
        present_ts_ns,
        dur_ns=present_dur_ns,
        tid=scenic_tid,
    )
    gfx_fxt_test_utils.add_vsync(
        builder, vsync_ts_ns, dur_ns=vsync_dur_ns, tid=vsync_tid
    )
    gfx_fxt_test_utils.add_flow(
        builder,
        "gfx",
        f"PresentFlow{app_flow_id}",
        app_flow_id,
        _APP_PID,
        app_tid,
        flow_begin_ts_ns + 500,
        _SCENIC_PID,
        scenic_tid,
        present_ts_ns + 500,
    )
    gfx_fxt_test_utils.add_flow(
        builder,
        "gfx",
        f"ToVsync{vsync_flow_id}",
        vsync_flow_id,
        _SCENIC_PID,
        scenic_tid,
        present_ts_ns + 1_000,
        _DISPLAY_PID,
        vsync_tid,
        vsync_ts_ns + 1_000,
    )


class AppRenderLatencyMetricsProcessorTest(unittest.TestCase):
    """Tests for SQL-based AppRenderLatencyMetricsProcessor."""

    @parameterized.expand(
        [
            param(
                "time_series",
                aggregates_only=False,
                expected_results=[
                    _TCR(
                        label="AppRenderVsyncLatency",
                        unit=_U.milliseconds,
                        values=[0.005, 0.003, 0.002, 0.007, 0.009],
                    ),
                    _TCR(
                        label="AppFps",
                        unit=_U.framesPerSecond,
                        values=[
                            125000.0,
                            111111.11111111111,
                            66666.66666666667,
                            83333.33333333333,
                        ],
                    ),
                ],
            ),
            param(
                "aggregates",
                aggregates_only=True,
                expected_results=(
                    trace_utils.standard_metrics_set(
                        [0.005, 0.003, 0.002, 0.007, 0.009],
                        "AppRenderVsyncLatency",
                        _U.milliseconds,
                    )
                    + trace_utils.standard_metrics_set(
                        [
                            125000.0,
                            111111.11111111111,
                            66666.66666666667,
                            83333.33333333333,
                        ],
                        "AppFps",
                        _U.framesPerSecond,
                    )
                ),
            ),
        ]
    )
    def test_process_metrics_with_fxt(
        self,
        _: str,
        aggregates_only: bool,
        expected_results: list[_TCR],
    ) -> None:
        """Verifies FXT SQL app render latency uses incoming slice_out.ts and exact deltas."""
        processor = app_render_metrics.AppRenderLatencyMetricsProcessor(
            "flatland-view-provider-example",
            aggregates_only=aggregates_only,
        )
        fxt_path = test_utils.runtime_dep_path("app_render_metric.fxt")
        with mock.patch.object(
            PerfettoTraceProcessor,
            "run_query",
            autospec=True,
            wraps=PerfettoTraceProcessor.run_query,
        ) as spy_run_query:
            actual_results = list(processor.process_metrics_with_fxt(fxt_path))

        self.assertEqual(spy_run_query.call_count, 2)
        queries = [call.args[1] for call in spy_run_query.call_args_list]
        self.assertNotIn(_gfx_sql.FALLBACK_VSYNC_QUERY, queries)

        docs = processor.describe(actual_results)
        self.assertEqual(docs["classname"], processor.__class__.__name__)
        self.assertNotEqual(docs["doc"], "")
        self.assertNotEqual(docs["code_path"], "")

        self.maxDiff = 10000
        self.assertSequenceEqual(actual_results, expected_results)

    def test_empty_present_trace(self) -> None:
        """Returns [] when the FXT trace contains no PerAppPresent slices."""
        processor = app_render_metrics.AppRenderLatencyMetricsProcessor(
            "flatland-view-provider-example",
            aggregates_only=False,
        )
        fxt_path = test_utils.runtime_dep_path("cpu_metric.fxt")
        self.assertSequenceEqual(
            processor.process_metrics_with_fxt(fxt_path), ()
        )

    def test_legacy_model_methods_return_empty(self) -> None:
        """Legacy model-based hooks return empty sets/list while keeping signatures."""
        processor = app_render_metrics.AppRenderLatencyMetricsProcessor(
            "flatland-view-provider-example"
        )
        self.assertEqual(processor.event_patterns, set())
        self.assertEqual(processor.category_names, set())
        self.assertSequenceEqual(
            processor.process_metrics(trace_model.Model([], {})), ()
        )

    def test_synthetic_fallback_and_common_start_time(self) -> None:
        """Covers slice.ts fallback, chronological OnVsync fallback, and common_process_start_time."""
        processor = app_render_metrics.AppRenderLatencyMetricsProcessor(
            "test-app", aggregates_only=False
        )
        builder = gfx_fxt_test_utils.new_gfx_builder(include_app=True)

        # Early PerAppPresent (10 us) before display starts (50 us) is skipped.
        _add_present(builder, "test-app", 10_000)
        gfx_fxt_test_utils.add_vsync(builder, 50_000)
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "EarlyFlow",
            1,
            _SCENIC_PID,
            _SCENIC_TID,
            12_000,
            _DISPLAY_PID,
            _DISPLAY_TID,
            52_000,
        )

        # Present 1: PresentBegin (100 us) -> PerAppPresent (110 us) -> OnVsync (150 us) => 0.05 ms.
        builder.add_duration_complete(
            "gfx",
            "FlatlandApp::PresentBegin",
            100_000,
            20_000,
            _APP_PID,
            _APP_TID,
        )
        _add_present(builder, "test-app", 110_000)
        gfx_fxt_test_utils.add_vsync(builder, 150_000)
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "PresentFlow",
            2,
            _APP_PID,
            _APP_TID,
            105_000,
            _SCENIC_PID,
            _SCENIC_TID,
            111_000,
        )
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "ToVsync",
            3,
            _SCENIC_PID,
            _SCENIC_TID,
            112_000,
            _DISPLAY_PID,
            _DISPLAY_TID,
            152_000,
        )

        # Present 2: no incoming flow (uses 200 us) and no outgoing flow -> fallback OnVsync (230 us).
        _add_present(builder, "test-app", 200_000)
        gfx_fxt_test_utils.add_vsync(builder, 230_000)

        self.assertSequenceEqual(
            gfx_fxt_test_utils.process_fxt(builder, processor),
            (
                _TCR(
                    label="AppRenderVsyncLatency",
                    unit=_U.milliseconds,
                    values=[0.05, 0.03],
                ),
                _TCR(
                    label="AppFps",
                    unit=_U.framesPerSecond,
                    # 1s / (230 us - 150 us) = 1s / 80_000 ns = 12500.0
                    values=[12500.0],
                ),
            ),
        )

    def test_post_draw_anchor_child_draw_slice_and_present_flow_begin(
        self,
    ) -> None:
        """Anchors latency post-draw for both legacy PresentBegin+draw and PresentFlowBegin."""
        processor = app_render_metrics.AppRenderLatencyMetricsProcessor(
            "test-app", aggregates_only=False
        )
        builder = gfx_fxt_test_utils.new_gfx_builder(include_app=True)
        builder.add_thread(_APP_PID, 4, "app-other")

        # Ensure Scenic and Display start before Frame 1 (100 us).
        builder.add_duration_complete(
            "gfx", "Init", 50_000, 5_000, _SCENIC_PID, _SCENIC_TID
        )
        gfx_fxt_test_utils.add_vsync(builder, 50_000)

        # Frame 1 (legacy flatland-rainbow trace):
        # FlatlandApp::PresentBegin [100, 120 us) wraps FlatlandViewProvider::draw
        # [101, 115 us) and an out-of-category child [115, 115.8 us); flow_begin
        # at 116 us fires post-draw from PresentBegin. start_ts must anchor to
        # end of draw (115 us) rather than PresentBegin.ts or Submit.end, so
        # OnVsync (150 us) -> 0.035 ms.
        builder.add_duration_complete(
            "gfx",
            "FlatlandApp::PresentBegin",
            100_000,
            20_000,
            _APP_PID,
            _APP_TID,
        )
        builder.add_duration_complete(
            "gfx",
            "FlatlandViewProvider::draw",
            101_000,
            14_000,
            _APP_PID,
            _APP_TID,
        )
        builder.add_duration_begin(
            "vulkan", "Submit", 115_000, _APP_PID, _APP_TID
        )
        builder.add_duration_end(
            "vulkan", "Submit", 115_800, _APP_PID, _APP_TID
        )
        _add_present(builder, "test-app", 117_000, dur_ns=2_000)
        gfx_fxt_test_utils.add_vsync(builder, 150_000)
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "PresentFlow1",
            1,
            _APP_PID,
            _APP_TID,
            116_000,
            _SCENIC_PID,
            _SCENIC_TID,
            117_500,
        )
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "ToVsync1",
            2,
            _SCENIC_PID,
            _SCENIC_TID,
            118_000,
            _DISPLAY_PID,
            _DISPLAY_TID,
            151_000,
        )

        # Frame 2 (new flatland-rainbow trace):
        # Dedicated leaf slice FlatlandApp::PresentFlowBegin [216, 217 us) nested
        # inside PresentBegin [200, 220 us) after draw [201, 215 us). Since
        # PresentFlowBegin has no children, start_ts resolves to 216 us;
        # OnVsync (250 us) -> 0.034 ms.
        builder.add_duration_complete(
            "gfx",
            "FlatlandApp::PresentBegin",
            200_000,
            20_000,
            _APP_PID,
            _APP_TID,
        )
        builder.add_duration_complete(
            "gfx",
            "FlatlandViewProvider::draw",
            201_000,
            14_000,
            _APP_PID,
            _APP_TID,
        )
        _add_present_frame(
            builder,
            "test-app",
            flow_begin_ts_ns=216_000,
            present_ts_ns=218_000,
            vsync_ts_ns=250_000,
            app_flow_id=3,
            vsync_flow_id=4,
            flow_begin_dur_ns=1_000,
            present_dur_ns=2_000,
        )

        # Frame 3: incoming flow from an out-of-category slice does not drop the
        # PerAppPresent row in LEFT JOIN and falls back to PerAppPresent.ts (300 us).
        builder.add_duration_complete(
            "input", "IgnoredInputSrc", 290_000, 5_000, _APP_PID, 4
        )
        _add_present(builder, "test-app", 300_000, dur_ns=5_000)
        gfx_fxt_test_utils.add_vsync(builder, 320_000)
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "OutOfCatSrcFlow",
            5,
            _APP_PID,
            4,
            291_000,
            _SCENIC_PID,
            _SCENIC_TID,
            301_000,
        )
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "ToVsync3",
            6,
            _SCENIC_PID,
            _SCENIC_TID,
            302_000,
            _DISPLAY_PID,
            _DISPLAY_TID,
            321_000,
        )

        # Frame 4: open-ended PresentBegin wrapping draw [401, 415 us) and an
        # unterminated dur=-1 child at 418 us; c.dur >= 0 ignores the dur=-1
        # child so start_ts stays 415 us -> 0.035 ms.
        builder.add_duration_begin(
            "gfx", "FlatlandApp::PresentBegin", 400_000, _APP_PID, _APP_TID
        )
        builder.add_duration_begin(
            "gfx", "FlatlandViewProvider::draw", 401_000, _APP_PID, _APP_TID
        )
        builder.add_duration_end(
            "gfx", "FlatlandViewProvider::draw", 415_000, _APP_PID, _APP_TID
        )
        builder.add_duration_begin(
            "gfx", "UnterminatedChild", 418_000, _APP_PID, _APP_TID
        )
        _add_present(builder, "test-app", 417_000, dur_ns=2_000)
        gfx_fxt_test_utils.add_vsync(builder, 450_000)
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "PresentFlow4",
            7,
            _APP_PID,
            _APP_TID,
            416_000,
            _SCENIC_PID,
            _SCENIC_TID,
            417_500,
        )
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "ToVsync4",
            8,
            _SCENIC_PID,
            _SCENIC_TID,
            418_000,
            _DISPLAY_PID,
            _DISPLAY_TID,
            451_000,
        )

        self.assertSequenceEqual(
            gfx_fxt_test_utils.process_fxt(builder, processor),
            (
                _TCR(
                    label="AppRenderVsyncLatency",
                    unit=_U.milliseconds,
                    values=[0.035, 0.034, 0.02, 0.035],
                ),
                _TCR(
                    label="AppFps",
                    unit=_U.framesPerSecond,
                    values=[
                        10_000.0,
                        1_000_000_000.0 / 70_000.0,
                        1_000_000_000.0 / 130_000.0,
                    ],
                ),
            ),
        )

    def test_insufficient_vsyncs_returns_empty_with_warning(self) -> None:
        """Logs a warning and returns [] for 0 vsyncs, 1 vsync, or squashed 0 FPS."""
        processor = app_render_metrics.AppRenderLatencyMetricsProcessor(
            "test-app", aggregates_only=False
        )

        # Case 1: Present events exist, but no vsyncs match (flow or fallback).
        builder_no_vsync = gfx_fxt_test_utils.new_gfx_builder(include_app=True)
        _add_present(builder_no_vsync, "test-app", 100_000)
        with self.assertLogs(app_render_metrics._LOGGER, level="WARNING"):
            self.assertSequenceEqual(
                gfx_fxt_test_utils.process_fxt(builder_no_vsync, processor), ()
            )

        # Case 2: Only 1 valid vsync.
        builder_one_vsync = gfx_fxt_test_utils.new_gfx_builder(include_app=True)
        _add_present(builder_one_vsync, "test-app", 100_000)
        gfx_fxt_test_utils.add_vsync(builder_one_vsync, 150_000)
        gfx_fxt_test_utils.add_flow(
            builder_one_vsync,
            "gfx",
            "ToVsync",
            1,
            _SCENIC_PID,
            _SCENIC_TID,
            101_000,
            _DISPLAY_PID,
            _DISPLAY_TID,
            151_000,
        )
        with self.assertLogs(app_render_metrics._LOGGER, level="WARNING"):
            self.assertSequenceEqual(
                gfx_fxt_test_utils.process_fxt(builder_one_vsync, processor), ()
            )

        # Case 3: 2 presents squashed into the same vsync timestamp (0 FPS samples).
        builder_squashed = gfx_fxt_test_utils.new_gfx_builder(include_app=True)
        gfx_fxt_test_utils.add_vsync(builder_squashed, 50_000)
        _add_present(builder_squashed, "test-app", 100_000)
        _add_present(builder_squashed, "test-app", 110_000)
        gfx_fxt_test_utils.add_vsync(builder_squashed, 150_000)
        gfx_fxt_test_utils.add_flow(
            builder_squashed,
            "gfx",
            "Squashed1",
            1,
            _SCENIC_PID,
            _SCENIC_TID,
            101_000,
            _DISPLAY_PID,
            _DISPLAY_TID,
            151_000,
        )
        gfx_fxt_test_utils.add_flow(
            builder_squashed,
            "gfx",
            "Squashed2",
            2,
            _SCENIC_PID,
            _SCENIC_TID,
            111_000,
            _DISPLAY_PID,
            _DISPLAY_TID,
            152_000,
        )
        with self.assertLogs(app_render_metrics._LOGGER, level="WARNING"):
            self.assertSequenceEqual(
                gfx_fxt_test_utils.process_fxt(builder_squashed, processor), ()
            )

    def test_present_duration_filter_and_fallback_vsync_boundaries(
        self,
    ) -> None:
        """Excludes dur=0/dur=-1 PerAppPresent slices and skips dur=-1 in fallback OnVsync."""
        processor = app_render_metrics.AppRenderLatencyMetricsProcessor(
            "test-app", aggregates_only=False
        )
        builder = gfx_fxt_test_utils.new_gfx_builder(include_app=True)
        builder.add_thread(_DISPLAY_PID, 3, "display-vsync")
        builder.add_thread(_APP_PID, 11, "unterminated-vsync")
        builder.add_thread(_APP_PID, 12, "unterminated-present")
        builder.add_thread(_SCENIC_PID, 14, "scenic-instant")

        # Ensure Display starts before Present 1 (100 us) for _common_start.
        gfx_fxt_test_utils.add_vsync(builder, 50_000, dur_ns=5_000, tid=3)

        # Present 1 (100 us) flows to OnVsync (150 us).
        _add_present(builder, "test-app", 100_000, dur_ns=10_000)
        gfx_fxt_test_utils.add_vsync(builder, 150_000, dur_ns=10_000, tid=3)
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "Flow1",
            1,
            _SCENIC_PID,
            _SCENIC_TID,
            102_000,
            _DISPLAY_PID,
            3,
            155_000,
        )

        # Present 2 (400 us) has no flows; fallback skips dur=-1 (410 us) and picks dur=0 (420 us).
        _add_present(builder, "test-app", 400_000)
        builder.add_duration_begin(
            "gfx", "Flatland::DisplayCompositor::OnVsync", 410_000, _APP_PID, 11
        )
        builder.add_instant(
            "gfx",
            "Flatland::DisplayCompositor::OnVsync",
            420_000,
            _DISPLAY_PID,
            _DISPLAY_TID,
        )
        # Instant (dur=0) and unterminated (dur=-1) PerAppPresent must be excluded
        # even when wired directly to complete OnVsync slices.
        builder.add_instant(
            "gfx", "Flatland::PerAppPresent[test-app]", 450_000, _SCENIC_PID, 14
        )
        gfx_fxt_test_utils.add_vsync(builder, 460_000, dur_ns=10_000, tid=3)
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "InstantPresentFlow",
            2,
            _SCENIC_PID,
            14,
            450_000,
            _DISPLAY_PID,
            3,
            462_000,
        )
        builder.add_duration_begin(
            "gfx", "Flatland::PerAppPresent[test-app]", 500_000, _APP_PID, 12
        )
        gfx_fxt_test_utils.add_vsync(builder, 520_000, dur_ns=10_000, tid=3)
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "UnterminatedPresentFlow",
            3,
            _APP_PID,
            12,
            501_000,
            _DISPLAY_PID,
            3,
            522_000,
        )

        self.assertSequenceEqual(
            gfx_fxt_test_utils.process_fxt(builder, processor),
            (
                _TCR(
                    label="AppRenderVsyncLatency",
                    unit=_U.milliseconds,
                    values=[0.05, 0.02],
                ),
                _TCR(
                    label="AppFps",
                    unit=_U.framesPerSecond,
                    values=[1_000_000_000.0 / 270_000.0],
                ),
            ),
        )

    def test_flow_vsync_anchored_to_start_ts(self) -> None:
        """Anchors OnVsync >= start_ts (even before PerAppPresent.ts) and ignores < start_ts."""
        processor = app_render_metrics.AppRenderLatencyMetricsProcessor(
            "test-app", aggregates_only=False
        )
        builder = gfx_fxt_test_utils.new_gfx_builder(include_app=True)
        builder.add_thread(_DISPLAY_PID, 5, "frame")
        builder.add_thread(_DISPLAY_PID, 6, "display-vsync")

        builder.add_duration_complete(
            "gfx", "Init", 50_000, 5_000, _APP_PID, _APP_TID
        )
        builder.add_duration_complete(
            "gfx", "Init", 50_000, 5_000, _SCENIC_PID, _SCENIC_TID
        )
        builder.add_duration_complete(
            "gfx", "Init", 50_000, 5_000, _DISPLAY_PID, 6
        )

        # Frame 1: PresentFlowBegin (100 us) -> PerAppPresent (130 us).
        # OnVsync (90 us < 100 us) via Frame is ignored; OnVsync (120 us, between
        # start_ts=100 us and PerAppPresent.ts=130 us) is matched => 0.02 ms.
        builder.add_duration_complete(
            "gfx", "Frame", 80_000, 80_000, _DISPLAY_PID, 5
        )
        gfx_fxt_test_utils.add_vsync(builder, 90_000, dur_ns=5_000, tid=6)
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "EarlyCommit",
            1,
            _DISPLAY_PID,
            5,
            85_000,
            _DISPLAY_PID,
            6,
            92_000,
        )
        builder.add_duration_complete(
            "gfx",
            "FlatlandApp::PresentFlowBegin",
            100_000,
            2_000,
            _APP_PID,
            _APP_TID,
        )
        gfx_fxt_test_utils.add_vsync(builder, 120_000, dur_ns=25_000, tid=6)
        _add_present(builder, "test-app", 130_000, dur_ns=10_000)
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "PresentFlow1",
            2,
            _APP_PID,
            _APP_TID,
            101_000,
            _SCENIC_PID,
            _SCENIC_TID,
            131_000,
        )
        builder.add_flow_begin(
            "gfx", "Dispatch1", 132_000, _SCENIC_PID, _SCENIC_TID, 3
        )
        builder.add_instant("gfx", "Receive1", 133_000, _DISPLAY_PID, 5)
        builder.add_flow_end("gfx", "Dispatch1", 133_000, _DISPLAY_PID, 5, 3)
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "ValidCommit",
            4,
            _DISPLAY_PID,
            5,
            135_000,
            _DISPLAY_PID,
            6,
            138_000,
        )

        # Frame 2: PresentFlowBegin (200 us) -> PerAppPresent (210 us) -> OnVsync (250 us).
        _add_present_frame(
            builder,
            "test-app",
            flow_begin_ts_ns=200_000,
            present_ts_ns=210_000,
            vsync_ts_ns=250_000,
            app_flow_id=5,
            vsync_flow_id=6,
            present_dur_ns=10_000,
            vsync_dur_ns=5_000,
            vsync_tid=6,
        )

        self.assertSequenceEqual(
            gfx_fxt_test_utils.process_fxt(builder, processor),
            (
                _TCR(
                    label="AppRenderVsyncLatency",
                    unit=_U.milliseconds,
                    values=[0.02, 0.05],
                ),
                _TCR(
                    label="AppFps",
                    unit=_U.framesPerSecond,
                    values=[1_000_000_000.0 / 130_000.0],
                ),
            ),
        )

    def test_latency_rounded_to_three_decimals(self) -> None:
        """Rounds AppRenderVsyncLatency values to 3 decimal places in milliseconds."""
        processor = app_render_metrics.AppRenderLatencyMetricsProcessor(
            "test-app", aggregates_only=False
        )
        builder = gfx_fxt_test_utils.new_gfx_builder(include_app=True)
        gfx_fxt_test_utils.add_vsync(builder, 50_000)

        # Frame 1: 112_345 - 100_000 = 12_345 ns = 0.012345 ms -> 0.012 ms.
        _add_present(builder, "test-app", 100_000, dur_ns=5_000)
        gfx_fxt_test_utils.add_vsync(builder, 112_345, dur_ns=5_000)
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "Flow1",
            1,
            _SCENIC_PID,
            _SCENIC_TID,
            101_000,
            _DISPLAY_PID,
            _DISPLAY_TID,
            113_000,
        )

        # Frame 2: 215_678 - 200_000 = 15_678 ns = 0.015678 ms -> 0.016 ms.
        _add_present(builder, "test-app", 200_000, dur_ns=5_000)
        gfx_fxt_test_utils.add_vsync(builder, 215_678, dur_ns=5_000)
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "Flow2",
            2,
            _SCENIC_PID,
            _SCENIC_TID,
            201_000,
            _DISPLAY_PID,
            _DISPLAY_TID,
            216_000,
        )

        self.assertSequenceEqual(
            gfx_fxt_test_utils.process_fxt(builder, processor),
            (
                _TCR(
                    label="AppRenderVsyncLatency",
                    unit=_U.milliseconds,
                    values=[0.012, 0.016],
                ),
                _TCR(
                    label="AppFps",
                    unit=_U.framesPerSecond,
                    values=[1_000_000_000.0 / (215_678 - 112_345)],
                ),
            ),
        )

    def test_app_filtering_quotes_equal_vsync_and_trailing_unmatched_fallback(
        self,
    ) -> None:
        """Filters other apps, escapes quotes, allows vsync==start, and drops trailing unmatched."""
        processor = app_render_metrics.AppRenderLatencyMetricsProcessor(
            "app'one", aggregates_only=False
        )
        builder = gfx_fxt_test_utils.new_gfx_builder(include_app=True)
        builder.add_thread(_SCENIC_PID, 2, "scenic-other")

        # Display starts at 50 us before app'one (100 us).
        gfx_fxt_test_utils.add_vsync(builder, 50_000)

        # Other app's PerAppPresent must be excluded.
        _add_present(builder, "other-app", 80_000, tid=2)
        gfx_fxt_test_utils.add_vsync(builder, 180_000)
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "OtherFlow",
            10,
            _SCENIC_PID,
            2,
            81_000,
            _DISPLAY_PID,
            _DISPLAY_TID,
            181_000,
        )

        # Present 1 for app'one: vsync_ts == start_ts (100 us) -> 0.0 ms.
        _add_present(builder, "app'one", 100_000, dur_ns=10_000)
        gfx_fxt_test_utils.add_vsync(builder, 100_000, dur_ns=10_000)
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "Flow1",
            1,
            _SCENIC_PID,
            _SCENIC_TID,
            102_000,
            _DISPLAY_PID,
            _DISPLAY_TID,
            105_000,
        )

        # Present 2 for app'one (200 us) -> OnVsync (250 us) => 0.05 ms.
        _add_present(builder, "app'one", 200_000, dur_ns=10_000)
        gfx_fxt_test_utils.add_vsync(builder, 250_000, dur_ns=10_000)
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "Flow2",
            2,
            _SCENIC_PID,
            _SCENIC_TID,
            202_000,
            _DISPLAY_PID,
            _DISPLAY_TID,
            252_000,
        )

        # Present 3 for app'one (300 us): unmatched present whose start_ts equals
        # a fallback OnVsync (300 us, followed by 340 us) -> 0.0 ms latency.
        _add_present(builder, "app'one", 300_000, dur_ns=10_000)
        gfx_fxt_test_utils.add_vsync(builder, 300_000, dur_ns=10_000)
        gfx_fxt_test_utils.add_vsync(builder, 340_000, dur_ns=10_000)

        # Present 4 for app'one (400 us): trailing unmatched present -> skipped.
        _add_present(builder, "app'one", 400_000, dur_ns=10_000)

        self.assertSequenceEqual(
            gfx_fxt_test_utils.process_fxt(builder, processor),
            (
                _TCR(
                    label="AppRenderVsyncLatency",
                    unit=_U.milliseconds,
                    values=[0.0, 0.05, 0.0],
                ),
                _TCR(
                    label="AppFps",
                    unit=_U.framesPerSecond,
                    values=[
                        1_000_000_000.0 / 150_000.0,
                        1_000_000_000.0 / 50_000.0,
                    ],
                ),
            ),
        )

    def test_multiple_incoming_flows_and_non_present_begin_children(
        self,
    ) -> None:
        """Selects MIN anchor across two incoming gfx flows and ignores children of non-PresentBegin sources."""
        processor = app_render_metrics.AppRenderLatencyMetricsProcessor(
            "test-app", aggregates_only=False
        )
        builder = gfx_fxt_test_utils.new_gfx_builder(include_app=True)
        builder.add_thread(_APP_PID, 4, "app-second")

        # Ensure Scenic and Display start before Frame 1 (100 us).
        builder.add_duration_complete(
            "gfx", "Init", 50_000, 5_000, _SCENIC_PID, _SCENIC_TID
        )
        gfx_fxt_test_utils.add_vsync(builder, 50_000)

        # Frame 1: single PerAppPresent (120 us) with TWO incoming gfx flows:
        # - Flow 1 from FlatlandApp::PresentFlowBegin at 112 us (emitted first).
        # - Flow 2 from FlatlandApp::PresentBegin [100, 118 us) wrapping draw [102, 108 us).
        # MIN(112 us, 108 us) = 108 us -> OnVsync (150 us) => 0.042 ms.
        builder.add_duration_complete(
            "gfx",
            "FlatlandApp::PresentFlowBegin",
            112_000,
            1_000,
            _APP_PID,
            4,
        )
        builder.add_duration_begin(
            "gfx",
            "FlatlandApp::PresentBegin",
            100_000,
            _APP_PID,
            _APP_TID,
        )
        builder.add_duration_begin(
            "gfx",
            "FlatlandViewProvider::draw",
            102_000,
            _APP_PID,
            _APP_TID,
        )
        builder.add_duration_end(
            "gfx",
            "FlatlandViewProvider::draw",
            108_000,
            _APP_PID,
            _APP_TID,
        )
        _add_present(builder, "test-app", 120_000, dur_ns=5_000)
        gfx_fxt_test_utils.add_vsync(builder, 150_000)
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "IncomingFlowA",
            1,
            _APP_PID,
            4,
            112_500,
            _SCENIC_PID,
            _SCENIC_TID,
            121_000,
        )
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "IncomingFlowB",
            2,
            _APP_PID,
            _APP_TID,
            110_000,
            _SCENIC_PID,
            _SCENIC_TID,
            122_000,
        )
        builder.add_duration_end(
            "gfx",
            "FlatlandApp::PresentBegin",
            118_000,
            _APP_PID,
            _APP_TID,
        )
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "ToVsync1",
            3,
            _SCENIC_PID,
            _SCENIC_TID,
            123_000,
            _DISPLAY_PID,
            _DISPLAY_TID,
            151_000,
        )

        # Frame 2: non-PresentBegin source slice (`Carnelian::Draw` at 200 us)
        # has a gfx child ending at 215 us; child-end shifting only applies to
        # FlatlandApp::PresentBegin, so start_ts stays 200 us -> 250 us = 0.05 ms.
        builder.add_duration_begin(
            "gfx",
            "Carnelian::Draw",
            200_000,
            _APP_PID,
            _APP_TID,
        )
        builder.add_duration_begin(
            "gfx",
            "Carnelian::Render",
            201_000,
            _APP_PID,
            _APP_TID,
        )
        builder.add_duration_end(
            "gfx",
            "Carnelian::Render",
            215_000,
            _APP_PID,
            _APP_TID,
        )
        _add_present(builder, "test-app", 222_000, dur_ns=5_000)
        gfx_fxt_test_utils.add_vsync(builder, 250_000)
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "CarnelianFlow",
            4,
            _APP_PID,
            _APP_TID,
            216_000,
            _SCENIC_PID,
            _SCENIC_TID,
            223_000,
        )
        builder.add_duration_end(
            "gfx",
            "Carnelian::Draw",
            220_000,
            _APP_PID,
            _APP_TID,
        )
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "ToVsync2",
            5,
            _SCENIC_PID,
            _SCENIC_TID,
            224_000,
            _DISPLAY_PID,
            _DISPLAY_TID,
            251_000,
        )

        self.assertSequenceEqual(
            gfx_fxt_test_utils.process_fxt(builder, processor),
            (
                _TCR(
                    label="AppRenderVsyncLatency",
                    unit=_U.milliseconds,
                    values=[0.042, 0.05],
                ),
                _TCR(
                    label="AppFps",
                    unit=_U.framesPerSecond,
                    values=[10_000.0],
                ),
            ),
        )

    def test_pre_common_start_incoming_gfx_flow_skips_present(self) -> None:
        """Skips a post-common-start PerAppPresent whose incoming gfx flow started before common start."""
        processor = app_render_metrics.AppRenderLatencyMetricsProcessor(
            "test-app", aggregates_only=False
        )
        builder = gfx_fxt_test_utils.new_gfx_builder(include_app=True)

        # Scenic starts at 50 us and Display starts at 100 us, so common_start = 100 us.
        builder.add_duration_complete(
            "gfx", "ScenicInit", 50_000, 5_000, _SCENIC_PID, _SCENIC_TID
        )
        gfx_fxt_test_utils.add_vsync(builder, 100_000)

        # Present 1 is at 110 us (>= 100 us), but its incoming gfx flow started
        # at 40 us (< 100 us). It must be skipped rather than falling back to 110 us.
        _add_present_frame(
            builder,
            "test-app",
            flow_begin_ts_ns=40_000,
            present_ts_ns=110_000,
            vsync_ts_ns=150_000,
            app_flow_id=1,
            vsync_flow_id=2,
            flow_begin_dur_ns=5_000,
        )

        # Present 2 (flow from 205 us -> 250 us => 0.045 ms) and Present 3
        # (flow from 305 us -> 360 us => 0.055 ms) are kept.
        _add_present_frame(
            builder,
            "test-app",
            flow_begin_ts_ns=205_000,
            present_ts_ns=210_000,
            vsync_ts_ns=250_000,
            app_flow_id=3,
            vsync_flow_id=4,
        )
        _add_present_frame(
            builder,
            "test-app",
            flow_begin_ts_ns=305_000,
            present_ts_ns=310_000,
            vsync_ts_ns=360_000,
            app_flow_id=5,
            vsync_flow_id=6,
        )

        self.assertSequenceEqual(
            gfx_fxt_test_utils.process_fxt(builder, processor),
            (
                _TCR(
                    label="AppRenderVsyncLatency",
                    unit=_U.milliseconds,
                    values=[0.045, 0.055],
                ),
                _TCR(
                    label="AppFps",
                    unit=_U.framesPerSecond,
                    values=[1_000_000_000.0 / 110_000.0],
                ),
            ),
        )
