#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for fps.py."""

import unittest

import gfx_fxt_test_utils
import test_utils
import trace_processing.metrics.fps as fps_metrics
from parameterized import param, parameterized
from reporting import metrics
from trace_processing import trace_model, trace_utils

_U = metrics.Unit
_TCR = metrics.TestCaseResult

_SCENIC_PID = gfx_fxt_test_utils.SCENIC_PID
_SCENIC_TID = gfx_fxt_test_utils.SCENIC_TID
_DISPLAY_PID = gfx_fxt_test_utils.DISPLAY_PID
_DISPLAY_TID = gfx_fxt_test_utils.DISPLAY_TID
_OTHER_PID = gfx_fxt_test_utils.OTHER_PID


class FpsMetricsProcessorTest(unittest.TestCase):
    """Tests for SQL-based FpsMetricsProcessor."""

    @parameterized.expand(
        [
            param(
                "time_series",
                aggregates_only=False,
                expected_results=[
                    _TCR(
                        label="Fps",
                        unit=_U.framesPerSecond,
                        values=[10000000.0, 5000000.0],
                    )
                ],
            ),
            param(
                "aggregates",
                aggregates_only=True,
                expected_results=trace_utils.standard_metrics_set(
                    [10000000.0, 5000000.0],
                    "Fps",
                    _U.framesPerSecond,
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
        """Verifies FXT SQL FPS matches legacy expectations."""
        processor = fps_metrics.FpsMetricsProcessor(
            aggregates_only=aggregates_only
        )
        fxt_path = test_utils.runtime_dep_path("fps_metric.fxt")
        actual_results = list(processor.process_metrics_with_fxt(fxt_path))

        docs = processor.describe(actual_results)
        self.assertEqual(docs["classname"], processor.__class__.__name__)
        self.assertNotEqual(docs["doc"], "")
        self.assertNotEqual(docs["code_path"], "")

        self.maxDiff = 10000
        self.assertSequenceEqual(actual_results, expected_results)

    def test_empty_fps_trace(self) -> None:
        """Returns [] when the FXT trace contains no RenderFrame slices."""
        processor = fps_metrics.FpsMetricsProcessor(aggregates_only=False)
        fxt_path = test_utils.runtime_dep_path("cpu_metric.fxt")
        self.assertSequenceEqual(
            processor.process_metrics_with_fxt(fxt_path), ()
        )

    def test_legacy_model_methods_return_empty(self) -> None:
        """Legacy model-based hooks return empty sets/list while keeping signatures."""
        processor = fps_metrics.FpsMetricsProcessor()
        self.assertEqual(processor.event_patterns, set())
        self.assertEqual(processor.category_names, set())
        self.assertSequenceEqual(
            processor.process_metrics(trace_model.Model([], {})), ()
        )

    def test_synthetic_squashed_frames_and_common_start(self) -> None:
        """Skips pre-common-start frames, unmatched RenderFrames, and duplicate vsyncs."""
        processor = fps_metrics.FpsMetricsProcessor(aggregates_only=False)
        builder = gfx_fxt_test_utils.new_gfx_builder()

        # Early RenderFrame (10 us) before display starts (50 us) -> skipped.
        gfx_fxt_test_utils.add_render_frame(builder, 10_000, dur_ns=5_000)
        gfx_fxt_test_utils.add_vsync(builder, 50_000)
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "rf0",
            1,
            _SCENIC_PID,
            _SCENIC_TID,
            12_000,
            _DISPLAY_PID,
            _DISPLAY_TID,
            52_000,
        )

        # Frame 1 (100 us) -> OnVsync 200 us; Frame 2 (120 us) unmatched -> skipped;
        # Frame 3 (150 us) also flows to OnVsync 200 us (squashed duplicate).
        gfx_fxt_test_utils.add_render_frame(builder, 100_000)
        gfx_fxt_test_utils.add_render_frame(builder, 120_000)
        gfx_fxt_test_utils.add_render_frame(builder, 150_000)
        gfx_fxt_test_utils.add_vsync(builder, 200_000)
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "rf1",
            2,
            _SCENIC_PID,
            _SCENIC_TID,
            105_000,
            _DISPLAY_PID,
            _DISPLAY_TID,
            201_000,
        )
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "rf3",
            3,
            _SCENIC_PID,
            _SCENIC_TID,
            155_000,
            _DISPLAY_PID,
            _DISPLAY_TID,
            202_000,
        )

        # Frame 4 (250 us) -> OnVsync 300 us.
        gfx_fxt_test_utils.add_render_frame(builder, 250_000)
        gfx_fxt_test_utils.add_vsync(builder, 300_000)
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "rf4",
            4,
            _SCENIC_PID,
            _SCENIC_TID,
            255_000,
            _DISPLAY_PID,
            _DISPLAY_TID,
            301_000,
        )

        # vsyncs: [200_000, 200_000, 300_000] -> duplicate skipped -> 1s / 100_000 ns = 10_000.0
        self.assertSequenceEqual(
            gfx_fxt_test_utils.process_fxt(builder, processor),
            (_TCR(label="Fps", unit=_U.framesPerSecond, values=[10000.0]),),
        )

    def test_insufficient_vsyncs_returns_empty_with_warning(self) -> None:
        """Returns [] with warning for 0 post-start renders, <2 vsyncs, or squashed 0 FPS."""
        processor = fps_metrics.FpsMetricsProcessor(aggregates_only=False)

        # Case 1: RenderFrame occurs before Display starts (no post-common-start RenderFrames).
        builder_no_post_start = gfx_fxt_test_utils.new_gfx_builder()
        gfx_fxt_test_utils.add_render_frame(
            builder_no_post_start, 10_000, dur_ns=5_000
        )
        gfx_fxt_test_utils.add_vsync(builder_no_post_start, 50_000)
        gfx_fxt_test_utils.add_flow(
            builder_no_post_start,
            "gfx",
            "rf_early",
            1,
            _SCENIC_PID,
            _SCENIC_TID,
            12_000,
            _DISPLAY_PID,
            _DISPLAY_TID,
            52_000,
        )
        with self.assertLogs(fps_metrics._LOGGER, level="WARNING") as logs:
            self.assertSequenceEqual(
                gfx_fxt_test_utils.process_fxt(
                    builder_no_post_start, processor
                ),
                (),
            )
        self.assertTrue(
            any(
                "found after the common process start time; cannot compute fps"
                in msg
                for msg in logs.output
            )
        )

        # Case 2: 2 RenderFrames, only 1 matched to an OnVsync.
        builder_one_vsync = gfx_fxt_test_utils.new_gfx_builder()
        gfx_fxt_test_utils.add_vsync(builder_one_vsync, 50_000)
        gfx_fxt_test_utils.add_render_frame(builder_one_vsync, 100_000)
        gfx_fxt_test_utils.add_render_frame(builder_one_vsync, 200_000)
        gfx_fxt_test_utils.add_vsync(builder_one_vsync, 150_000)
        gfx_fxt_test_utils.add_flow(
            builder_one_vsync,
            "gfx",
            "rf1",
            1,
            _SCENIC_PID,
            _SCENIC_TID,
            102_000,
            _DISPLAY_PID,
            _DISPLAY_TID,
            151_000,
        )
        with self.assertLogs(fps_metrics._LOGGER, level="WARNING"):
            self.assertSequenceEqual(
                gfx_fxt_test_utils.process_fxt(builder_one_vsync, processor), ()
            )

        # Case 3: 2 RenderFrames squashed onto the same OnVsync timestamp (0 FPS).
        builder_squashed = gfx_fxt_test_utils.new_gfx_builder()
        gfx_fxt_test_utils.add_vsync(builder_squashed, 50_000)
        gfx_fxt_test_utils.add_render_frame(builder_squashed, 100_000)
        gfx_fxt_test_utils.add_render_frame(builder_squashed, 110_000)
        gfx_fxt_test_utils.add_vsync(builder_squashed, 150_000)
        gfx_fxt_test_utils.add_flow(
            builder_squashed,
            "gfx",
            "rf1",
            1,
            _SCENIC_PID,
            _SCENIC_TID,
            102_000,
            _DISPLAY_PID,
            _DISPLAY_TID,
            151_000,
        )
        gfx_fxt_test_utils.add_flow(
            builder_squashed,
            "gfx",
            "rf2",
            2,
            _SCENIC_PID,
            _SCENIC_TID,
            112_000,
            _DISPLAY_PID,
            _DISPLAY_TID,
            152_000,
        )
        with self.assertLogs(fps_metrics._LOGGER, level="WARNING"):
            self.assertSequenceEqual(
                gfx_fxt_test_utils.process_fxt(builder_squashed, processor), ()
            )

    def test_render_frame_duration_filter(self) -> None:
        """Excludes dur=0 instant and dur=-1 unterminated RenderFrame slices."""
        processor = fps_metrics.FpsMetricsProcessor(aggregates_only=False)
        builder = gfx_fxt_test_utils.new_gfx_builder()
        builder.add_thread(_DISPLAY_PID, 3, "display-vsync")
        builder.add_thread(_SCENIC_PID, 14, "instant-render")
        builder.add_process(_OTHER_PID, "other")
        builder.add_thread(_OTHER_PID, 12, "unterminated-render")

        gfx_fxt_test_utils.add_vsync(builder, 50_000, dur_ns=10_000, tid=3)

        # Valid RenderFrame 1 (100 us) -> OnVsync (200 us).
        gfx_fxt_test_utils.add_render_frame(builder, 100_000, dur_ns=20_000)
        gfx_fxt_test_utils.add_vsync(builder, 200_000, dur_ns=10_000, tid=3)
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "Valid1",
            1,
            _SCENIC_PID,
            _SCENIC_TID,
            102_000,
            _DISPLAY_PID,
            3,
            205_000,
        )

        # Valid RenderFrame 2 (300 us) -> OnVsync (400 us).
        gfx_fxt_test_utils.add_render_frame(builder, 300_000, dur_ns=20_000)
        gfx_fxt_test_utils.add_vsync(builder, 400_000, dur_ns=10_000, tid=3)
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "Valid2",
            2,
            _SCENIC_PID,
            _SCENIC_TID,
            302_000,
            _DISPLAY_PID,
            3,
            405_000,
        )

        # Instant (dur=0) and unterminated (dur=-1) RenderFrames flowing to
        # OnVsync (500 us, 600 us) must be excluded.
        builder.add_instant("gfx", "RenderFrame", 450_000, _SCENIC_PID, 14)
        gfx_fxt_test_utils.add_vsync(builder, 500_000, dur_ns=10_000, tid=3)
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "InstantFlow",
            3,
            _SCENIC_PID,
            14,
            450_000,
            _DISPLAY_PID,
            3,
            505_000,
        )
        builder.add_duration_begin(
            "gfx", "RenderFrame", 550_000, _OTHER_PID, 12
        )
        gfx_fxt_test_utils.add_vsync(builder, 600_000, dur_ns=10_000, tid=3)
        gfx_fxt_test_utils.add_flow(
            builder,
            "gfx",
            "UnterminatedFlow",
            4,
            _OTHER_PID,
            12,
            552_000,
            _DISPLAY_PID,
            3,
            605_000,
        )

        self.assertSequenceEqual(
            gfx_fxt_test_utils.process_fxt(builder, processor),
            (_TCR(label="Fps", unit=_U.framesPerSecond, values=[5000.0]),),
        )
