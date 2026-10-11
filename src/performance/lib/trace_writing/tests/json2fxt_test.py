# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for json2fxt.py."""

import json
import os
import pathlib
import struct
import tempfile
import unittest

from json2fxt import FxtConverter


class Json2FxtTest(unittest.TestCase):
    """Test suite validating JSON to FXT conversion."""

    def test_magic_and_initialization(self) -> None:
        converter = FxtConverter()
        with tempfile.TemporaryDirectory() as tmp_dir:
            json_path = os.path.join(tmp_dir, "test.json")
            fxt_path = os.path.join(tmp_dir, "test.fxt")

            with open(json_path, "w") as f:
                json.dump({"traceEvents": []}, f)

            converter.convert(json_path, fxt_path)

            with open(fxt_path, "rb") as f:
                content = f.read()

            # Word 0: Magic number (0x0016547846040010)
            magic = struct.unpack_from("<Q", content, 0)[0]
            self.assertEqual(magic, 0x0016547846040010)

            # Word 1: Initialization record header
            # Type 1 (Initialization), length 2 words
            init_header = struct.unpack_from("<Q", content, 8)[0]
            self.assertEqual(init_header & 0xF, 1)
            self.assertEqual((init_header >> 4) & 0xFFF, 2)

            # Word 2: Ticks per second (1 GHz)
            ticks_per_sec = struct.unpack_from("<Q", content, 16)[0]
            self.assertEqual(ticks_per_sec, 1000000000)

    def test_golden_cpu_metric_conversion(self) -> None:
        """Verify that converting cpu_metric.json produces identical bytes to cpu_metric.fxt."""
        runtime_deps = (
            pathlib.Path(__file__).resolve().parent.parent.parent
            / "runtime_deps"
        )
        json_path = runtime_deps / "cpu_metric.json"
        golden_fxt_path = runtime_deps / "cpu_metric.fxt"
        if not json_path.exists() or not golden_fxt_path.exists():
            self.skipTest(f"Test data not found in {runtime_deps}")

        converter = FxtConverter()
        with tempfile.TemporaryDirectory() as tmp_dir:
            out_fxt_path = os.path.join(tmp_dir, "cpu_metric.fxt")
            converter.convert(json_path, out_fxt_path)

            with open(golden_fxt_path, "rb") as f_golden, open(
                out_fxt_path, "rb"
            ) as f_out:
                self.assertEqual(f_golden.read(), f_out.read())

    def test_argument_types(self) -> None:
        """Verify encoding of various argument types (int32, int64, float, str, bool, process)."""
        converter = FxtConverter()
        # int32
        arg_i32 = converter._pack_argument("int32_val", 42)
        self.assertGreater(len(arg_i32), 0)
        # int64
        arg_i64 = converter._pack_argument("int64_val", 0x100000000)
        self.assertGreater(len(arg_i64), 0)
        # float
        arg_float = converter._pack_argument("float_val", 3.14)
        self.assertGreater(len(arg_float), 0)
        # str
        arg_str = converter._pack_argument("str_val", "hello")
        self.assertGreater(len(arg_str), 0)
        # bool
        arg_bool = converter._pack_argument("bool_val", True)
        self.assertGreater(len(arg_bool), 0)
        # Verify bool header type is 9
        header = struct.unpack_from("<Q", arg_bool, 0)[0]
        self.assertEqual(header & 0xF, 9)
        # process koid
        arg_proc = converter._pack_argument("process", 1234)
        self.assertGreater(len(arg_proc), 0)
        proc_header = struct.unpack_from("<Q", arg_proc, 0)[0]
        self.assertEqual(proc_header & 0xF, 8)

    def test_string_too_long_raises(self) -> None:
        converter = FxtConverter()
        too_long = "x" * 0x8000
        with self.assertRaises(ValueError):
            converter._make_string_ref(too_long)

    def test_synthetic_events_conversion(self) -> None:
        """Verify that synthetic trace with all event types converts without error."""
        synthetic_trace = {
            "traceEvents": [
                {
                    "ph": "M",
                    "name": "process_name",
                    "pid": 100,
                    "args": {"name": "test_proc"},
                },
                {
                    "ph": "M",
                    "name": "thread_name",
                    "pid": 100,
                    "tid": 200,
                    "args": {"name": "test_thread"},
                },
                {
                    "ph": "X",
                    "cat": "category",
                    "name": "duration_complete",
                    "ts": 1000.0,
                    "dur": 500.0,
                    "pid": 100,
                    "tid": 200,
                    "args": {"count": 1, "flag": True},
                },
                {
                    "ph": "B",
                    "cat": "category",
                    "name": "duration_begin",
                    "ts": 2000.0,
                    "pid": 100,
                    "tid": 200,
                },
                {
                    "ph": "E",
                    "cat": "category",
                    "name": "duration_end",
                    "ts": 2500.0,
                    "pid": 100,
                    "tid": 200,
                },
                {
                    "ph": "i",
                    "cat": "category",
                    "name": "instant_global",
                    "ts": 3000.0,
                    "s": "g",
                    "pid": 100,
                    "tid": 200,
                },
                {
                    "ph": "i",
                    "cat": "category",
                    "name": "instant_thread",
                    "ts": 3100.0,
                    "s": "t",
                    "pid": 100,
                    "tid": 200,
                },
                {
                    "ph": "C",
                    "cat": "category",
                    "name": "counter_event",
                    "ts": 4000.0,
                    "pid": 100,
                    "tid": 200,
                    "args": {"series_a": 10},
                },
                {
                    "ph": "b",
                    "cat": "category",
                    "name": "async_event",
                    "ts": 5000.0,
                    "id": 42,
                    "pid": 100,
                    "tid": 200,
                },
                {
                    "ph": "e",
                    "cat": "category",
                    "name": "async_event",
                    "ts": 5500.0,
                    "id": 42,
                    "pid": 100,
                    "tid": 200,
                },
                {
                    "ph": "s",
                    "cat": "category",
                    "name": "flow_event",
                    "ts": 6000.0,
                    "id": "0xabc",
                    "pid": 100,
                    "tid": 200,
                },
                {
                    "ph": "f",
                    "cat": "category",
                    "name": "flow_event",
                    "ts": 6500.0,
                    "id": "0xabc",
                    "pid": 100,
                    "tid": 200,
                },
            ],
            "systemTraceEvents": {
                "events": [
                    {"ph": "p", "pid": 100, "name": "test_proc"},
                    {"ph": "t", "pid": 100, "tid": 200, "name": "test_thread"},
                    {"ph": "w", "cpu": 0, "ts": 100.0, "tid": 200},
                    {
                        "ph": "k",
                        "cpu": 0,
                        "ts": 105.0,
                        "out": {"tid": 200, "prio": 16, "state": 0},
                        "in": {"tid": 0, "prio": 0},
                    },
                ]
            },
        }

        converter = FxtConverter()
        with tempfile.TemporaryDirectory() as tmp_dir:
            json_path = os.path.join(tmp_dir, "synthetic.json")
            fxt_path = os.path.join(tmp_dir, "synthetic.fxt")

            with open(json_path, "w") as f:
                json.dump(synthetic_trace, f)

            converter.convert(json_path, fxt_path)

            self.assertTrue(os.path.exists(fxt_path))
            self.assertGreater(os.path.getsize(fxt_path), 0)


if __name__ == "__main__":
    unittest.main()
