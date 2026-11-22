#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for gpu.py."""

import json
import struct
import tempfile
import typing
import unittest
from collections.abc import Sequence

import gfx_fxt_test_utils
import test_utils
import trace_processing.metrics.gpu as gpu_metrics
from parameterized import param, parameterized
from reporting import metrics
from tp_shell import PerfettoTraceProcessor
from trace_processing import trace_model, trace_utils
from trace_writing import fxt

_U = metrics.Unit
_TCR = metrics.TestCaseResult


def _build_gpu_fxt(samples: Sequence[tuple[int, float]]) -> fxt.Builder:
    """Builds a binary FXT trace with GPU Utilization counter samples."""
    builder = fxt.Builder()
    builder.add_process(pid=1, name="magma")
    builder.add_thread(pid=1, tid=1, name="gpu")
    for timestamp_ns, utilization in samples:
        builder.add_counter(
            category="magma",
            name="GPU Utilization",
            timestamp_ns=timestamp_ns,
            pid=1,
            tid=1,
            args={"utilization": utilization},
            counter_id=1,
        )
    return builder


def _encode_varint(value: int) -> bytes:
    out = bytearray()
    while True:
        byte = value & 0x7F
        value >>= 7
        if value:
            out.append(byte | 0x80)
        else:
            out.append(byte)
            return bytes(out)


def _proto_varint(field_num: int, value: int) -> bytes:
    return _encode_varint((field_num << 3) | 0) + _encode_varint(value)


def _proto_bytes(field_num: int, payload: bytes) -> bytes:
    return (
        _encode_varint((field_num << 3) | 2)
        + _encode_varint(len(payload))
        + payload
    )


def _proto_double(field_num: int, value: float) -> bytes:
    return _encode_varint((field_num << 3) | 1) + struct.pack("<d", value)


def _build_perfetto_pb_counter_trace(
    track_name: str, samples: Sequence[tuple[int, float]]
) -> bytes:
    """Builds a minimal Perfetto proto trace with a bare-named CounterTrack."""
    track_uuid = 12345
    seq_id = 1
    track_descriptor = (
        _proto_varint(1, track_uuid)
        + _proto_bytes(2, track_name.encode("utf-8"))
        + _proto_bytes(8, b"")
    )
    packets = [
        _proto_bytes(
            1, _proto_varint(10, seq_id) + _proto_bytes(60, track_descriptor)
        )
    ]
    for ts_ns, value in samples:
        track_event = (
            _proto_varint(11, track_uuid)
            + _proto_varint(9, 4)  # TYPE_COUNTER
            + _proto_double(44, value)
        )
        packet = (
            _proto_varint(8, ts_ns)
            + _proto_varint(10, seq_id)
            + _proto_bytes(11, track_event)
        )
        packets.append(_proto_bytes(1, packet))
    return b"".join(packets)


class GpuMetricsProcessorTest(unittest.TestCase):
    """Tests for SQL-based GpuMetricsProcessor."""

    @parameterized.expand(
        [
            param(
                "time_series",
                aggregates_only=False,
                expected_results=(
                    _TCR(
                        label="GpuUtilization",
                        unit=_U.percent,
                        values=[20.0, 80.0, 40.0],
                    ),
                ),
            ),
            param(
                "aggregates",
                aggregates_only=True,
                expected_results=trace_utils.standard_metrics_set(
                    [20.0, 80.0, 40.0],
                    "Gpu",
                    _U.percent,
                    durations=[2_000_000.0, 1_000_000.0, 0.0],
                ),
            ),
        ]
    )
    def test_process_metrics_with_fxt_binary_counter(
        self,
        _: str,
        aggregates_only: bool,
        expected_results: Sequence[_TCR],
    ) -> None:
        """Verifies FXT counter track ('GPU Utilization:utilization:*') and filters unrelated tracks."""
        builder = _build_gpu_fxt(
            (
                (1_000_000, 0.2),
                (3_000_000, 0.8),
                (4_000_000, 0.4),
            )
        )
        # Unrelated FXT counter tracks must be ignored.
        builder.add_counter(
            category="magma",
            name="GPU Utilization",
            timestamp_ns=2_000_000,
            pid=1,
            tid=1,
            args={"frequency": 500.0},
            counter_id=2,
        )
        builder.add_counter(
            category="system_metrics",
            name="CPU Load",
            timestamp_ns=2_500_000,
            pid=1,
            tid=1,
            args={"utilization": 0.99},
            counter_id=3,
        )

        processor = gpu_metrics.GpuMetricsProcessor(
            aggregates_only=aggregates_only
        )
        with tempfile.NamedTemporaryFile(suffix=".fxt") as tmp:
            builder.write_to_file(tmp.name)
            with PerfettoTraceProcessor(trace_path=tmp.name) as tp:
                track_names = [
                    row["name"]
                    for row in tp.run_query(
                        "SELECT name FROM counter_track ORDER BY name ASC"
                    )
                ]
            self.assertIn("GPU Utilization:utilization:1", track_names)
            actual_results = list(processor.process_metrics_with_fxt(tmp.name))

        docs = processor.describe(actual_results)
        self.assertEqual(docs["classname"], processor.__class__.__name__)
        self.assertNotEqual(docs["doc"], "")
        self.assertNotEqual(docs["code_path"], "")
        self.assertSequenceEqual(actual_results, expected_results)

    def test_empty_gpu_trace(self) -> None:
        """Returns [] when the FXT trace contains no GPU Utilization counters."""
        processor = gpu_metrics.GpuMetricsProcessor(aggregates_only=False)
        fxt_path = test_utils.runtime_dep_path("cpu_metric.fxt")
        self.assertSequenceEqual(
            processor.process_metrics_with_fxt(fxt_path), ()
        )

    def test_single_sample_aggregates(self) -> None:
        """Handles a single GPU sample without dividing by zero duration."""
        processor = gpu_metrics.GpuMetricsProcessor(aggregates_only=True)
        builder = _build_gpu_fxt(((1_000_000, 0.5),))
        self.assertSequenceEqual(
            gfx_fxt_test_utils.process_fxt(builder, processor),
            trace_utils.standard_metrics_set([50.0], "Gpu", _U.percent),
        )

    def test_legacy_model_methods_return_empty(self) -> None:
        """Legacy model-based hooks return empty sets/list while keeping signatures."""
        processor = gpu_metrics.GpuMetricsProcessor()
        self.assertEqual(processor.event_patterns, set())
        self.assertEqual(processor.category_names, set())
        self.assertSequenceEqual(
            processor.process_metrics(trace_model.Model([], {})), ()
        )

    def test_chrome_json_id_counter_track_and_unrelated_filtered(self) -> None:
        """Supports Chrome-JSON 'id' counter tracks and filters unrelated counters."""
        processor = gpu_metrics.GpuMetricsProcessor(aggregates_only=False)
        trace_events: list[dict[str, typing.Any]] = [
            {
                "cat": "magma",
                "name": "GPU Utilization",
                "ph": "C",
                "pid": 1,
                "tid": 1,
                "id": "0x1",
                "ts": 1000,
                "args": {"utilization": 0.25},
            },
            {
                "cat": "magma",
                "name": "GPU Utilization",
                "ph": "C",
                "pid": 1,
                "tid": 1,
                "id": "0x1",
                "ts": 3000,
                "args": {"utilization": 0.75},
            },
            # Unrelated non-GPU counter tracks that must be filtered out.
            {
                "cat": "magma",
                "name": "GPU Utilization",
                "ph": "C",
                "pid": 1,
                "tid": 1,
                "id": "0x1",
                "ts": 2000,
                "args": {"frequency": 500},
            },
            {
                "cat": "system_metrics",
                "name": "CPU Load",
                "ph": "C",
                "pid": 1,
                "tid": 1,
                "ts": 2500,
                "args": {"utilization": 0.99},
            },
        ]
        with tempfile.NamedTemporaryFile(mode="w", suffix=".json") as tmp:
            json.dump({"traceEvents": trace_events}, tmp)
            tmp.flush()
            with PerfettoTraceProcessor(trace_path=tmp.name) as tp:
                track_names = [
                    row["name"]
                    for row in tp.run_query(
                        "SELECT name FROM counter_track ORDER BY name ASC"
                    )
                ]
            self.assertIn("GPU Utilization id: 0x1 utilization", track_names)
            results = list(processor.process_metrics_with_fxt(tmp.name))

        self.assertSequenceEqual(
            results,
            (
                _TCR(
                    label="GpuUtilization",
                    unit=_U.percent,
                    values=[25.0, 75.0],
                ),
            ),
        )

    def test_chrome_json_counter_track_without_id(self) -> None:
        """Supports Chrome-JSON counter tracks without 'id' ('GPU Utilization utilization')."""
        processor = gpu_metrics.GpuMetricsProcessor(aggregates_only=False)
        trace_events: list[dict[str, typing.Any]] = [
            {
                "cat": "magma",
                "name": "GPU Utilization",
                "ph": "C",
                "pid": 1,
                "tid": 1,
                "ts": 1000,
                "args": {"utilization": 0.3},
            },
            {
                "cat": "magma",
                "name": "GPU Utilization",
                "ph": "C",
                "pid": 1,
                "tid": 1,
                "ts": 3000,
                "args": {"utilization": 0.6},
            },
        ]
        with tempfile.NamedTemporaryFile(mode="w", suffix=".json") as tmp:
            json.dump({"traceEvents": trace_events}, tmp)
            tmp.flush()
            with PerfettoTraceProcessor(trace_path=tmp.name) as tp:
                track_names = [
                    row["name"]
                    for row in tp.run_query(
                        "SELECT name FROM counter_track ORDER BY name ASC"
                    )
                ]
            self.assertIn("GPU Utilization utilization", track_names)
            results = list(processor.process_metrics_with_fxt(tmp.name))

        self.assertSequenceEqual(
            results,
            (
                _TCR(
                    label="GpuUtilization",
                    unit=_U.percent,
                    values=[30.0, 60.0],
                ),
            ),
        )

    def test_perfetto_proto_bare_counter_track_name(self) -> None:
        """Supports bare 'GPU Utilization' counter_track names."""
        processor = gpu_metrics.GpuMetricsProcessor(aggregates_only=True)
        pb_bytes = _build_perfetto_pb_counter_trace(
            "GPU Utilization", ((1_000_000, 0.25), (3_000_000, 0.75))
        )
        with tempfile.NamedTemporaryFile(suffix=".pb") as tmp:
            tmp.write(pb_bytes)
            tmp.flush()
            with PerfettoTraceProcessor(trace_path=tmp.name) as tp:
                track_names = [
                    row["name"]
                    for row in tp.run_query(
                        "SELECT name FROM counter_track ORDER BY name ASC"
                    )
                ]
            self.assertIn("GPU Utilization", track_names)
            results = list(processor.process_metrics_with_fxt(tmp.name))

        self.assertSequenceEqual(
            results,
            trace_utils.standard_metrics_set(
                [25.0, 75.0],
                "Gpu",
                _U.percent,
                durations=[2_000_000.0, 0.0],
            ),
        )
