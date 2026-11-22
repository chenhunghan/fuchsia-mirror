#!/usr/bin/env fuchsia-vendored-python
# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for trace metrics processors."""

import inspect as py_inspect
import json
import pathlib
import tempfile
import typing
import unittest
from collections.abc import Collection
from unittest import mock

import test_utils
import trace_processing.metrics.cpu as cpu_metrics
import trace_processing.metrics.input_latency as input_latency_metrics
from parameterized import param, parameterized
from reporting import metrics
from tp_shell import PerfettoTraceProcessor
from trace_processing import (
    trace_importing,
    trace_metrics,
    trace_model,
    trace_utils,
)

# Boilerplate-busting constants:
U = metrics.Unit
TCR = metrics.TestCaseResult


def _process_synthetic_trace(
    trace_events: list[dict[str, typing.Any]],
) -> list[metrics.TestCaseResult]:
    """Runs the processor (time series mode) on a Chrome-JSON trace."""
    processor = input_latency_metrics.InputLatencyMetricsProcessor(
        aggregates_only=False
    )
    with tempfile.NamedTemporaryFile(mode="w", suffix=".json") as tmp:
        json.dump({"traceEvents": trace_events}, tmp)
        tmp.flush()
        return list(processor.process_metrics_with_fxt(tmp.name))


class TestCaseResultTest(unittest.TestCase):
    """Tests TestCaseResult"""

    def test_to_json(self) -> None:
        label = "L1"
        test_suite = "bar"

        result = TCR(
            label=label, unit=U.bytesPerSecond, values=[0, 0.1, 23.45, 6]
        )

        self.assertEqual(result.label, label)
        self.assertEqual(
            result.to_json(test_suite=test_suite),
            {
                "label": label,
                "test_suite": test_suite,
                "unit": "bytes/second",
                "values": [0, 0.1, 23.45, 6],
            },
        )

    def test_write_fuchsia_perf_json(self) -> None:
        test_suite = "ts"
        results = [
            TCR(label="l1", unit=U.percent, values=[1]),
            TCR(label="l2", unit=U.framesPerSecond, values=[2]),
        ]

        with tempfile.TemporaryDirectory() as tmpdir:
            actual_output_path = (
                pathlib.Path(tmpdir) / "actual_output.fuchsiaperf.json"
            )

            metrics.TestCaseResult.write_fuchsiaperf_json(
                results,
                test_suite=test_suite,
                output_path=actual_output_path,
            )

            actual_output = json.loads(actual_output_path.read_text())
            self.assertEqual(
                actual_output, [r.to_json(test_suite) for r in results]
            )


_METRICS_TEST_PARAMS = [
    param(
        "cpu",
        processor=cpu_metrics.CpuMetricsProcessor(aggregates_only=False),
        model_file="cpu_metric.json",
        expected_results=[
            TCR(label="CpuLoad", unit=U.percent, values=[43, 20]),
        ],
    ),
    param(
        "cpu_aggregates",
        processor=cpu_metrics.CpuMetricsProcessor(aggregates_only=True),
        model_file="cpu_metric.json",
        expected_results=trace_utils.standard_metrics_set(
            # ts in the json is in microseconds, durations is in ns.
            [43, 20],
            "Cpu",
            U.percent,
            durations=[1000000000, 1100000000],
        ),
    ),
]


class MetricProcessorsTest(unittest.TestCase):
    """Tests for the various MetricProcessors."""

    def _load_model(self, model_file_name: str) -> trace_model.Model:
        return trace_importing.create_model_from_file_path(
            test_utils.runtime_dep_path(model_file_name)
        )

    def _load_fxt_model(self, fxt_file: str) -> trace_model.Model:
        return trace_importing.create_model_from_trace_file_path(
            test_utils.runtime_dep_path(fxt_file),
            without_json_conversion=True,
        )

    @parameterized.expand(_METRICS_TEST_PARAMS)
    def test_processor(
        self,
        _: str,
        processor: trace_metrics.MetricsProcessor,
        model_file: str,
        expected_results: list[TCR],
    ) -> None:
        """Tests a processor's outputs with a given input model loaded from a json file"""
        model = self._load_model(model_file)
        actual_results = list(processor.process_metrics(model))

        docs = processor.describe(actual_results)
        self.assertEqual(
            docs.get("classname", ""), processor.__class__.__name__
        )
        self.assertNotEqual(docs["doc"], "")
        self.assertNotEqual(docs["code_path"], "")

        # Improves assertEqual output when comparing lists.
        self.maxDiff = 10000
        self.assertEqual(actual_results, expected_results)

    @parameterized.expand(_METRICS_TEST_PARAMS)
    def test_processor_fxt(
        self,
        _: str,
        processor: trace_metrics.MetricsProcessor,
        model_file: str,
        expected_results: list[TCR],
    ) -> None:
        """Tests a processor's outputs with a given input model loaded from an FXT file"""
        fxt_file = model_file.replace(".json", ".fxt")
        model = self._load_fxt_model(fxt_file)
        actual_results = list(processor.process_metrics(model))

        docs = processor.describe(actual_results)
        self.assertEqual(
            docs.get("classname", ""), processor.__class__.__name__
        )
        self.assertNotEqual(docs["doc"], "")
        self.assertNotEqual(docs["code_path"], "")

        self.maxDiff = 10000
        self.assertEqual(actual_results, expected_results)

    class TestMetricsProcessor(trace_metrics.MetricsProcessor):
        """TEST"""

        @typing.override
        def process_metrics(
            self, model: trace_model.Model
        ) -> Collection[metrics.TestCaseResult]:
            # If ever called, this can be implemented.
            raise NotImplementedError()

    def test_auto_doc(self) -> None:
        docs = self.TestMetricsProcessor().describe([])
        self.assertEqual(docs.get("classname", ""), "TestMetricsProcessor")
        self.assertEqual(docs["doc"], "TEST")
        self.assertEqual(docs["code_path"], py_inspect.getfile(self.__class__))


class InputLatencyMetricsProcessorTest(unittest.TestCase):
    """Tests for SQL-based InputLatencyMetricsProcessor."""

    @parameterized.expand(
        [
            param(
                "time_series",
                aggregates_only=False,
                expected_results=[
                    TCR(
                        label="total_input_latency",
                        unit=U.milliseconds,
                        values=[0.003, 0.002, 0.007, 0.009],
                    ),
                ],
            ),
            param(
                "aggregates",
                aggregates_only=True,
                expected_results=trace_utils.standard_metrics_set(
                    [0.003, 0.002, 0.007, 0.009],
                    "InputLatency",
                    U.milliseconds,
                ),
            ),
        ]
    )
    def test_process_metrics_with_fxt(
        self,
        _: str,
        aggregates_only: bool,
        expected_results: list[TCR],
    ) -> None:
        """Verifies FXT SQL input latency matches legacy expectations and registration filtering."""
        processor = input_latency_metrics.InputLatencyMetricsProcessor(
            aggregates_only=aggregates_only
        )
        fxt_path = test_utils.runtime_dep_path("input_latency_metric.fxt")
        with mock.patch.object(
            PerfettoTraceProcessor,
            "run_query",
            autospec=True,
            wraps=PerfettoTraceProcessor.run_query,
        ) as spy_run_query:
            actual_results = list(processor.process_metrics_with_fxt(fxt_path))

        self.assertEqual(spy_run_query.call_count, 2)
        queries = [call.args[1] for call in spy_run_query.call_args_list]
        self.assertNotIn(input_latency_metrics._FALLBACK_VSYNC_QUERY, queries)

        docs = processor.describe(actual_results)
        self.assertEqual(docs["classname"], processor.__class__.__name__)
        self.assertNotEqual(docs["doc"], "")
        self.assertNotEqual(docs["code_path"], "")

        self.maxDiff = 10000
        self.assertEqual(actual_results, expected_results)

    def test_empty_input_trace(self) -> None:
        """Returns [] when the FXT trace contains no input events."""
        processor = input_latency_metrics.InputLatencyMetricsProcessor(
            aggregates_only=False
        )
        fxt_path = test_utils.runtime_dep_path("cpu_metric.fxt")
        self.assertEqual(list(processor.process_metrics_with_fxt(fxt_path)), [])

    def test_legacy_model_methods_return_empty(self) -> None:
        """Legacy model-based hooks return empty sets/list while keeping signatures."""
        processor = input_latency_metrics.InputLatencyMetricsProcessor()
        self.assertEqual(processor.event_patterns, set())
        self.assertEqual(processor.category_names, set())
        self.assertEqual(
            list(processor.process_metrics(trace_model.Model([], {}))), []
        )

    def test_flow_vsync_query_mapping_on_fxt(self) -> None:
        """Directly asserts _FLOW_VSYNC_QUERY maps each filtered input slice to its flow OnVsync."""
        fxt_path = test_utils.runtime_dep_path("input_latency_metric.fxt")
        with PerfettoTraceProcessor(trace_path=fxt_path) as tp:
            input_rows = tp.run_query(input_latency_metrics._INPUT_EVENTS_QUERY)
            flow_rows = tp.run_query(input_latency_metrics._FLOW_VSYNC_QUERY)

        vsync_by_id = {
            int(row["input_id"]): int(row["vsync_ts"]) for row in flow_rows
        }
        self.assertEqual(
            [
                (int(row["ts"]), vsync_by_id[int(row["id"])])
                for row in input_rows
            ],
            [
                (109_000, 112_000),
                (119_000, 121_000),
                (129_000, 136_000),
                (139_000, 148_000),
            ],
        )

    def test_fallback_vsync_query_on_fxt(self) -> None:
        """Directly asserts _FALLBACK_VSYNC_QUERY returns ordered OnVsync timestamps on FXT."""
        fxt_path = test_utils.runtime_dep_path("input_latency_metric.fxt")
        with PerfettoTraceProcessor(trace_path=fxt_path) as tp:
            rows = tp.run_query(input_latency_metrics._FALLBACK_VSYNC_QUERY)

        self.assertEqual(
            [int(row["ts"]) for row in rows],
            [104_000, 112_000, 121_000, 136_000, 148_000],
        )

    def test_out_of_scope_category_flows_ignored(self) -> None:
        """Ignores flows and zero-length children outside gfx/input categories."""
        # Chrome-JSON timestamps are in microseconds (Perfetto scales by 1000).
        trace_events: list[dict[str, typing.Any]] = [
            # Case 1: input event encloses a dur=0 kernel:ipc ChannelMessage
            # that flows via kernel:ipc to another dur=0 ChannelMessage
            # inside a gfx RenderFrame flowing to an earlier OnVsync (130 us).
            # Category filtering must block that path so only the valid
            # input->gfx flow to OnVsync (200 us) matches (latency 0.1 ms).
            {
                "cat": "input",
                "name": "input-device-process-reports",
                "ph": "X",
                "pid": 1,
                "tid": 1,
                "ts": 100,
                "dur": 50,
            },
            {
                "cat": "input",
                "name": "Dispatch",
                "ph": "s",
                "bp": "e",
                "pid": 1,
                "tid": 1,
                "ts": 105,
                "id": 1,
            },
            {
                "cat": "kernel:ipc",
                "name": "ChannelMessage",
                "ph": "X",
                "pid": 1,
                "tid": 1,
                "ts": 110,
                "dur": 0,
            },
            {
                "cat": "kernel:ipc",
                "name": "ChannelFlow",
                "ph": "s",
                "bp": "e",
                "pid": 1,
                "tid": 1,
                "ts": 110,
                "id": 2,
            },
            {
                "cat": "gfx",
                "name": "RenderFrame",
                "ph": "X",
                "pid": 2,
                "tid": 2,
                "ts": 112,
                "dur": 15,
            },
            {
                "cat": "gfx",
                "name": "Commit",
                "ph": "s",
                "bp": "e",
                "pid": 2,
                "tid": 2,
                "ts": 113,
                "id": 3,
            },
            {
                "cat": "kernel:ipc",
                "name": "ChannelMessage",
                "ph": "X",
                "pid": 2,
                "tid": 2,
                "ts": 115,
                "dur": 0,
            },
            {
                "cat": "kernel:ipc",
                "name": "ChannelFlow",
                "ph": "f",
                "bp": "e",
                "pid": 2,
                "tid": 2,
                "ts": 115,
                "id": 2,
            },
            {
                "cat": "gfx",
                "name": "Flatland::DisplayCompositor::OnVsync",
                "ph": "X",
                "pid": 2,
                "tid": 3,
                "ts": 130,
                "dur": 10,
            },
            {
                "cat": "gfx",
                "name": "Commit",
                "ph": "f",
                "bp": "e",
                "pid": 2,
                "tid": 3,
                "ts": 135,
                "id": 3,
            },
            {
                "cat": "gfx",
                "name": "Flatland::DisplayCompositor::OnVsync",
                "ph": "X",
                "pid": 2,
                "tid": 3,
                "ts": 200,
                "dur": 10,
            },
            {
                "cat": "input",
                "name": "Dispatch",
                "ph": "f",
                "bp": "e",
                "pid": 2,
                "tid": 3,
                "ts": 205,
                "id": 1,
            },
            # Case 2: direct cross-category flow edges (input -> kernel:ipc
            # and kernel:ipc -> gfx) to an earlier OnVsync (335 us) must be
            # ignored so only the direct input->gfx flow to 400 us matches.
            {
                "cat": "input",
                "name": "input-device-process-reports",
                "ph": "X",
                "pid": 1,
                "tid": 1,
                "ts": 300,
                "dur": 50,
            },
            {
                "cat": "input",
                "name": "ValidDispatch2",
                "ph": "s",
                "bp": "e",
                "pid": 1,
                "tid": 1,
                "ts": 302,
                "id": 4,
            },
            {
                "cat": "input",
                "name": "CrossCatIn",
                "ph": "s",
                "bp": "e",
                "pid": 1,
                "tid": 1,
                "ts": 305,
                "id": 5,
            },
            {
                "cat": "kernel:ipc",
                "name": "IpcBridge",
                "ph": "X",
                "pid": 2,
                "tid": 2,
                "ts": 310,
                "dur": 20,
            },
            {
                "cat": "input",
                "name": "CrossCatIn",
                "ph": "f",
                "bp": "e",
                "pid": 2,
                "tid": 2,
                "ts": 315,
                "id": 5,
            },
            {
                "cat": "gfx",
                "name": "CrossCatOut",
                "ph": "s",
                "bp": "e",
                "pid": 2,
                "tid": 2,
                "ts": 320,
                "id": 6,
            },
            {
                "cat": "gfx",
                "name": "Flatland::DisplayCompositor::OnVsync",
                "ph": "X",
                "pid": 2,
                "tid": 3,
                "ts": 335,
                "dur": 10,
            },
            {
                "cat": "gfx",
                "name": "CrossCatOut",
                "ph": "f",
                "bp": "e",
                "pid": 2,
                "tid": 3,
                "ts": 340,
                "id": 6,
            },
            {
                "cat": "gfx",
                "name": "Flatland::DisplayCompositor::OnVsync",
                "ph": "X",
                "pid": 2,
                "tid": 3,
                "ts": 400,
                "dur": 10,
            },
            {
                "cat": "input",
                "name": "ValidDispatch2",
                "ph": "f",
                "bp": "e",
                "pid": 2,
                "tid": 3,
                "ts": 405,
                "id": 4,
            },
        ]

        results = _process_synthetic_trace(trace_events)

        self.assertEqual(
            results,
            [
                TCR(
                    label="total_input_latency",
                    unit=U.milliseconds,
                    values=[0.1, 0.1],
                )
            ],
        )

    def test_flow_vsync_before_input_ignored(self) -> None:
        """Ignores flow-reachable OnVsync slices that start before the input event."""
        # Chrome-JSON timestamps are in microseconds (Perfetto scales by 1000).
        # The input flow lands on a dur=0 slice inside a longer gfx slice. The
        # dur=0 child -> enclosing parent edge reaches the parent's earlier
        # flow to an OnVsync at 70 us, which precedes the input event (100 us)
        # and must not be selected; only the OnVsync at 300 us matches.
        results = _process_synthetic_trace(
            [
                {
                    "cat": "gfx",
                    "name": "Frame",
                    "ph": "X",
                    "pid": 2,
                    "tid": 5,
                    "ts": 50,
                    "dur": 200,
                },
                {
                    "cat": "gfx",
                    "name": "EarlyCommit",
                    "ph": "s",
                    "bp": "e",
                    "pid": 2,
                    "tid": 5,
                    "ts": 60,
                    "id": 1,
                },
                {
                    "cat": "gfx",
                    "name": "Flatland::DisplayCompositor::OnVsync",
                    "ph": "X",
                    "pid": 2,
                    "tid": 6,
                    "ts": 70,
                    "dur": 10,
                },
                {
                    "cat": "gfx",
                    "name": "EarlyCommit",
                    "ph": "f",
                    "bp": "e",
                    "pid": 2,
                    "tid": 6,
                    "ts": 75,
                    "id": 1,
                },
                {
                    "cat": "input",
                    "name": "input-device-process-reports",
                    "ph": "X",
                    "pid": 1,
                    "tid": 1,
                    "ts": 100,
                    "dur": 50,
                },
                {
                    "cat": "input",
                    "name": "Dispatch",
                    "ph": "s",
                    "bp": "e",
                    "pid": 1,
                    "tid": 1,
                    "ts": 105,
                    "id": 2,
                },
                {
                    "cat": "gfx",
                    "name": "Receive",
                    "ph": "i",
                    "s": "t",
                    "pid": 2,
                    "tid": 5,
                    "ts": 160,
                },
                {
                    "cat": "input",
                    "name": "Dispatch",
                    "ph": "f",
                    "bp": "e",
                    "pid": 2,
                    "tid": 5,
                    "ts": 160,
                    "id": 2,
                },
                {
                    "cat": "gfx",
                    "name": "LateCommit",
                    "ph": "s",
                    "bp": "e",
                    "pid": 2,
                    "tid": 5,
                    "ts": 170,
                    "id": 3,
                },
                {
                    "cat": "gfx",
                    "name": "Flatland::DisplayCompositor::OnVsync",
                    "ph": "X",
                    "pid": 2,
                    "tid": 6,
                    "ts": 300,
                    "dur": 10,
                },
                {
                    "cat": "gfx",
                    "name": "LateCommit",
                    "ph": "f",
                    "bp": "e",
                    "pid": 2,
                    "tid": 6,
                    "ts": 305,
                    "id": 3,
                },
            ]
        )

        self.assertEqual(
            results,
            [
                TCR(
                    label="total_input_latency",
                    unit=U.milliseconds,
                    values=[0.2],
                )
            ],
        )

    def test_duration_filter_boundaries(self) -> None:
        """Exercises dur = 0 instant and dur = -1 unterminated slice filtering."""
        # Chrome-JSON timestamps are in microseconds (Perfetto scales by 1000).
        trace_events: list[dict[str, typing.Any]] = [
            # Input 1 (100 us) flows to a dur=0 instant OnVsync (200 us), which
            # is not a flow target but is non-terminal and flows on to a
            # complete OnVsync (300 us). Input 1 also flows to an unterminated
            # (dur=-1) OnVsync (250 us), which must not be selected.
            {
                "cat": "input",
                "name": "input-device-process-reports",
                "ph": "X",
                "pid": 1,
                "tid": 1,
                "ts": 100,
                "dur": 50,
            },
            {
                "cat": "input",
                "name": "ToInstant",
                "ph": "s",
                "bp": "e",
                "pid": 1,
                "tid": 1,
                "ts": 105,
                "id": 1,
            },
            {
                "cat": "input",
                "name": "ToUnterminated",
                "ph": "s",
                "bp": "e",
                "pid": 1,
                "tid": 1,
                "ts": 106,
                "id": 2,
            },
            {
                "cat": "input",
                "name": "ToTerminalOnVsync",
                "ph": "s",
                "bp": "e",
                "pid": 1,
                "tid": 1,
                "ts": 107,
                "id": 6,
            },
            # Complete (dur > 0) OnVsync starting before Input 1 (90 us) with
            # an outgoing flow (to 250 us) and a child slice with a flow (to
            # 260 us). Both must be pruned so Input 1 (reaching via a dur=0
            # instant inside OnVsync 90) still selects 300 us.
            {
                "cat": "gfx",
                "name": "Flatland::DisplayCompositor::OnVsync",
                "ph": "X",
                "pid": 2,
                "tid": 4,
                "ts": 90,
                "dur": 30,
            },
            {
                "cat": "gfx",
                "name": "FromTerminalVsync",
                "ph": "s",
                "bp": "e",
                "pid": 2,
                "tid": 4,
                "ts": 95,
                "id": 4,
            },
            {
                "cat": "gfx",
                "name": "ChildOfTerminalVsync",
                "ph": "X",
                "pid": 2,
                "tid": 4,
                "ts": 96,
                "dur": 5,
            },
            {
                "cat": "gfx",
                "name": "FromTerminalChild",
                "ph": "s",
                "bp": "e",
                "pid": 2,
                "tid": 4,
                "ts": 98,
                "id": 5,
            },
            {
                "cat": "gfx",
                "name": "TerminalVsyncInstant",
                "ph": "i",
                "s": "t",
                "pid": 2,
                "tid": 4,
                "ts": 108,
            },
            {
                "cat": "input",
                "name": "ToTerminalOnVsync",
                "ph": "f",
                "bp": "e",
                "pid": 2,
                "tid": 4,
                "ts": 108,
                "id": 6,
            },
            {
                "cat": "gfx",
                "name": "Flatland::DisplayCompositor::OnVsync",
                "ph": "X",
                "pid": 2,
                "tid": 5,
                "ts": 250,
                "dur": 10,
            },
            {
                "cat": "gfx",
                "name": "FromTerminalVsync",
                "ph": "f",
                "bp": "e",
                "pid": 2,
                "tid": 5,
                "ts": 255,
                "id": 4,
            },
            {
                "cat": "gfx",
                "name": "Flatland::DisplayCompositor::OnVsync",
                "ph": "X",
                "pid": 2,
                "tid": 6,
                "ts": 260,
                "dur": 10,
            },
            {
                "cat": "gfx",
                "name": "FromTerminalChild",
                "ph": "f",
                "bp": "e",
                "pid": 2,
                "tid": 6,
                "ts": 265,
                "id": 5,
            },
            {
                "cat": "gfx",
                "name": "Flatland::DisplayCompositor::OnVsync",
                "ph": "i",
                "s": "t",
                "pid": 2,
                "tid": 2,
                "ts": 200,
            },
            {
                "cat": "input",
                "name": "ToInstant",
                "ph": "f",
                "bp": "e",
                "pid": 2,
                "tid": 2,
                "ts": 200,
                "id": 1,
            },
            {
                "cat": "gfx",
                "name": "FromInstant",
                "ph": "s",
                "bp": "e",
                "pid": 2,
                "tid": 2,
                "ts": 200,
                "id": 3,
            },
            {
                "cat": "gfx",
                "name": "Flatland::DisplayCompositor::OnVsync",
                "ph": "X",
                "pid": 2,
                "tid": 3,
                "ts": 300,
                "dur": 10,
            },
            {
                "cat": "gfx",
                "name": "FromInstant",
                "ph": "f",
                "bp": "e",
                "pid": 2,
                "tid": 3,
                "ts": 305,
                "id": 3,
            },
            # Input 2 (400 us) has no flows. The chronological fallback must
            # skip the unterminated OnVsync (410 us) and accept the dur=0
            # instant OnVsync (420 us).
            {
                "cat": "input",
                "name": "input-device-process-reports",
                "ph": "X",
                "pid": 1,
                "tid": 1,
                "ts": 400,
                "dur": 5,
            },
            {
                "cat": "gfx",
                "name": "Flatland::DisplayCompositor::OnVsync",
                "ph": "i",
                "s": "t",
                "pid": 2,
                "tid": 2,
                "ts": 420,
            },
            # Unterminated slices, each on its own thread so they do not
            # enclose other slices.
            {
                "cat": "gfx",
                "name": "Flatland::DisplayCompositor::OnVsync",
                "ph": "B",
                "pid": 3,
                "tid": 10,
                "ts": 250,
            },
            {
                "cat": "input",
                "name": "ToUnterminated",
                "ph": "f",
                "bp": "e",
                "pid": 3,
                "tid": 10,
                "ts": 255,
                "id": 2,
            },
            {
                "cat": "gfx",
                "name": "Flatland::DisplayCompositor::OnVsync",
                "ph": "B",
                "pid": 3,
                "tid": 11,
                "ts": 410,
            },
            # An unterminated input event must not be measured.
            {
                "cat": "input",
                "name": "input-device-process-reports",
                "ph": "B",
                "pid": 3,
                "tid": 12,
                "ts": 500,
            },
            # An unterminated Register must not filter out earlier inputs.
            {
                "cat": "input",
                "name": "PointerinjectorRegistry::Register",
                "ph": "B",
                "pid": 3,
                "tid": 13,
                "ts": 600,
            },
            # dur = 0 instant input and Register slices must be ignored.
            {
                "cat": "input",
                "name": "input-device-process-reports",
                "ph": "i",
                "s": "t",
                "pid": 1,
                "tid": 14,
                "ts": 450,
            },
            {
                "cat": "input",
                "name": "PointerinjectorRegistry::Register",
                "ph": "i",
                "s": "t",
                "pid": 1,
                "tid": 15,
                "ts": 650,
            },
        ]

        with tempfile.NamedTemporaryFile(mode="w", suffix=".json") as tmp:
            json.dump({"traceEvents": trace_events}, tmp)
            tmp.flush()
            with PerfettoTraceProcessor(trace_path=tmp.name) as tp:
                input_rows = tp.run_query(
                    input_latency_metrics._INPUT_EVENTS_QUERY
                )
                flow_rows = tp.run_query(
                    input_latency_metrics._FLOW_VSYNC_QUERY
                )
                fallback_rows = tp.run_query(
                    input_latency_metrics._FALLBACK_VSYNC_QUERY
                )

        input_ts_by_id = {int(row["id"]): int(row["ts"]) for row in input_rows}
        self.assertEqual(sorted(input_ts_by_id.values()), [100_000, 400_000])
        self.assertEqual(
            [
                (input_ts_by_id[int(row["input_id"])], int(row["vsync_ts"]))
                for row in flow_rows
            ],
            [(100_000, 300_000)],
        )
        self.assertEqual(
            [int(row["ts"]) for row in fallback_rows],
            [90_000, 200_000, 250_000, 260_000, 300_000, 420_000],
        )

        self.assertEqual(
            _process_synthetic_trace(trace_events),
            [
                TCR(
                    label="total_input_latency",
                    unit=U.milliseconds,
                    # Input 1: flow 300 us - 100 us = 0.2 ms.
                    # Input 2: fallback 420 us - 400 us = 0.02 ms.
                    values=[0.2, 0.02],
                )
            ],
        )

    @mock.patch.object(
        input_latency_metrics, "PerfettoTraceProcessor", autospec=True
    )
    def test_no_input_events_short_circuits(self, mock_cls: mock.Mock) -> None:
        """Skips flow/fallback queries when no input events remain after Register."""
        processor = input_latency_metrics.InputLatencyMetricsProcessor(
            aggregates_only=False
        )
        mock_tp = mock_cls.return_value.__enter__.return_value
        mock_tp.run_query.return_value = []

        results = list(processor.process_metrics_with_fxt("trace.fxt"))

        self.assertEqual(results, [])
        mock_cls.assert_called_once_with(trace_path="trace.fxt")
        mock_tp.run_query.assert_called_once()

    @mock.patch.object(
        input_latency_metrics, "PerfettoTraceProcessor", autospec=True
    )
    def test_all_flow_matched_skips_fallback_query(
        self, mock_cls: mock.Mock
    ) -> None:
        """Avoids querying fallback OnVsync timestamps when all events match via flow."""
        processor = input_latency_metrics.InputLatencyMetricsProcessor(
            aggregates_only=False
        )
        mock_tp = mock_cls.return_value.__enter__.return_value
        mock_tp.run_query.side_effect = [
            [
                {"id": 10, "ts": 100_000},
                {"id": 20, "ts": 200_000},
            ],
            [
                {"input_id": 10, "vsync_ts": 104_000},
                {"input_id": 20, "vsync_ts": 206_000},
            ],
        ]

        results = list(processor.process_metrics_with_fxt("trace.fxt"))

        self.assertEqual(
            results,
            [
                TCR(
                    label="total_input_latency",
                    unit=U.milliseconds,
                    values=[0.004, 0.006],
                )
            ],
        )
        self.assertEqual(mock_tp.run_query.call_count, 2)

    @mock.patch.object(
        input_latency_metrics, "PerfettoTraceProcessor", autospec=True
    )
    def test_chronological_fallback(self, mock_cls: mock.Mock) -> None:
        """Falls back to earliest OnVsync >= input_ts via bisect_left for unmatched events."""
        processor = input_latency_metrics.InputLatencyMetricsProcessor(
            aggregates_only=False
        )
        mock_tp = mock_cls.return_value.__enter__.return_value
        mock_tp.run_query.side_effect = [
            [
                {"id": 1, "ts": 100_000},
                {"id": 2, "ts": 200_000},
                {"id": 3, "ts": 250_000},
                {"id": 4, "ts": 500_000},
            ],
            [
                {"input_id": 1, "vsync_ts": 105_000},
            ],
            [
                {"ts": 90_000},
                {"ts": 105_000},
                {"ts": 212_000},
                {"ts": 250_000},
                {"ts": 300_000},
            ],
        ]

        results = list(processor.process_metrics_with_fxt("trace.fxt"))

        self.assertEqual(
            results,
            [
                TCR(
                    label="total_input_latency",
                    unit=U.milliseconds,
                    # id=1: flow 105_000 - 100_000 = 0.005ms
                    # id=2: fallback 212_000 - 200_000 = 0.012ms
                    # id=3: exact boundary ts == 250_000 -> 0.0ms
                    # id=4: no OnVsync >= 500_000 -> skipped
                    values=[0.005, 0.012, 0.0],
                )
            ],
        )
        self.assertEqual(mock_tp.run_query.call_count, 3)

    @mock.patch.object(
        input_latency_metrics, "PerfettoTraceProcessor", autospec=True
    )
    def test_unmatched_without_any_vsync(self, mock_cls: mock.Mock) -> None:
        """Returns [] when unmatched input events have no following OnVsync."""
        processor = input_latency_metrics.InputLatencyMetricsProcessor(
            aggregates_only=False
        )
        mock_tp = mock_cls.return_value.__enter__.return_value
        mock_tp.run_query.side_effect = [
            [{"id": 1, "ts": 100_000}],
            [],
            [],
        ]

        results = list(processor.process_metrics_with_fxt("trace.fxt"))

        self.assertEqual(results, [])
        self.assertEqual(mock_tp.run_query.call_count, 3)
