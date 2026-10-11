# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for trace_writing.fxt."""

import io
import os
import struct
import tempfile
import unittest

from trace_processing import trace_importing_fxt, trace_model
from trace_writing import fxt


def create_model_from_fxt_builder(builder: fxt.Builder) -> trace_model.Model:
    """Create a trace model from an fxt.Builder via a temporary file."""
    with tempfile.TemporaryDirectory() as tmp_dir:
        temp_path = os.path.join(tmp_dir, "trace.fxt")
        with open(temp_path, "wb") as f:
            f.write(builder.to_bytes())
        return trace_importing_fxt.create_model_from_fxt_path_directly(
            temp_path
        )


class FxtTest(unittest.TestCase):
    """Test suite validating fxt.Builder functionality."""

    def test_magic_and_initialization(self) -> None:
        builder = fxt.Builder(ticks_per_second=1000000000)
        data = builder.to_bytes()

        self.assertGreaterEqual(len(data), 24)
        magic = struct.unpack_from("<Q", data, 0)[0]
        self.assertEqual(magic, 0x0016547846040010)

        init_header = struct.unpack_from("<Q", data, 8)[0]
        self.assertEqual(init_header & 0xF, 1)  # Type 1 = Initialization
        self.assertEqual((init_header >> 4) & 0xFFF, 2)  # Size 2 words

        ticks = struct.unpack_from("<Q", data, 16)[0]
        self.assertEqual(ticks, 1000000000)

    def test_argument_types(self) -> None:
        builder = fxt.Builder()
        builder.add_process(pid=100, name="test_process")
        builder.add_thread(pid=100, tid=200, name="test_thread")
        builder.add_duration_complete(
            category="test_cat",
            name="test_event",
            timestamp_ns=1000,
            duration_ns=500,
            pid=100,
            tid=200,
            args={
                "int_val": 42,
                "int64_val": 0x100000000,
                "neg_int64_val": -10000000000,
                "float_val": 3.14,
                "str_val": "hello",
                "bool_val": True,
                "process": 100,
            },
        )
        model = create_model_from_fxt_builder(builder)
        events = list(model.all_events())
        self.assertEqual(len(events), 1)
        ev = events[0]
        self.assertEqual(ev.name, "test_event")
        self.assertEqual(ev.args["int_val"], 42)
        self.assertEqual(ev.args["int64_val"], 0x100000000)
        self.assertEqual(ev.args["neg_int64_val"], -10000000000)
        self.assertAlmostEqual(ev.args["float_val"], 3.14)
        self.assertEqual(ev.args["str_val"], "hello")
        self.assertEqual(ev.args["bool_val"], True)

    def test_end_to_end_model_creation(self) -> None:
        """Verify that fxt.Builder produces a valid trace model."""
        builder = fxt.Builder()
        builder.add_process(pid=100, name="my_process")
        builder.add_thread(pid=100, tid=200, name="my_thread")
        builder.add_duration_complete(
            category="benchmark",
            name="slice_a",
            timestamp_ns=1000,
            duration_ns=5000,
            pid=100,
            tid=200,
            args={"key": "value"},
        )
        builder.add_instant(
            category="benchmark",
            name="instant_mark",
            timestamp_ns=2000,
            pid=100,
            tid=200,
            scope="t",
        )

        model: trace_model.Model = create_model_from_fxt_builder(builder)

        processes = {p.pid: p.name for p in model.processes}
        self.assertIn(100, processes)
        self.assertEqual(processes[100], "my_process")

        events = list(model.all_events())
        event_names = [e.name for e in events]
        self.assertIn("slice_a", event_names)
        self.assertIn("instant_mark", event_names)

    def test_all_event_types(self) -> None:
        builder = fxt.Builder()
        builder.add_process(pid=100, name="proc")
        builder.add_thread(pid=100, tid=200, name="thread")
        builder.add_duration_begin(
            category="cat", name="dur_b", timestamp_ns=1000, pid=100, tid=200
        )
        builder.add_duration_end(
            category="cat", name="dur_b", timestamp_ns=2000, pid=100, tid=200
        )
        builder.add_instant(
            category="cat",
            name="inst_p",
            timestamp_ns=3000,
            pid=100,
            tid=200,
            scope="p",
        )
        builder.add_counter(
            category="cat",
            name="counter",
            timestamp_ns=4000,
            pid=100,
            tid=200,
            args={"metric": 10},
        )
        model = create_model_from_fxt_builder(builder)
        events = list(model.all_events())
        self.assertEqual(len(events), 3)  # duration, instant, counter
        names = [e.name for e in events]
        self.assertIn("dur_b", names)
        self.assertIn("inst_p", names)
        self.assertIn("counter", names)

    def test_flow_and_async_events(self) -> None:
        builder = fxt.Builder()
        builder.add_process(pid=100, name="proc")
        builder.add_thread(pid=100, tid=200, name="thread")
        builder.add_flow_begin("cat", "flow", 1000, 100, 200, flow_id=1)
        builder.add_flow_step("cat", "flow", 2000, 100, 200, flow_id=1)
        builder.add_flow_end("cat", "flow", 3000, 100, 200, flow_id=1)
        builder.add_async_begin("cat", "async", 1000, 100, 200, async_id=1)
        builder.add_async_instant("cat", "async", 2000, 100, 200, async_id=1)
        builder.add_async_end("cat", "async", 3000, 100, 200, async_id=1)
        data = builder.to_bytes()
        self.assertGreater(len(data), 0)

    def test_scheduler_events(self) -> None:
        builder = fxt.Builder()
        builder.add_process(pid=100, name="proc")
        builder.add_thread(pid=100, tid=200, name="thread")
        builder.add_thread_wakeup(timestamp_ns=1000, cpu_number=0, tid=200)
        builder.add_context_switch(
            timestamp_ns=2000,
            cpu_number=0,
            outgoing_thread_state=2,
            outgoing_tid=200,
            incoming_tid=0,
            outgoing_priority=10,
            incoming_priority=0,
        )
        model = create_model_from_fxt_builder(builder)
        self.assertIn(0, model.scheduling_records)
        records = model.scheduling_records[0]
        self.assertEqual(len(records), 2)
        self.assertIsInstance(records[0], trace_model.Waking)
        self.assertIsInstance(records[1], trace_model.ContextSwitch)

    def test_string_too_long_raises(self) -> None:
        builder = fxt.Builder()
        too_long = "x" * 0x8000
        with self.assertRaises(ValueError):
            builder.add_process(pid=1, name=too_long)

    def test_thread_limit_raises(self) -> None:
        builder = fxt.Builder()
        builder.add_process(pid=1, name="proc")
        # Add 255 threads (valid indices 1..255)
        for i in range(1, 256):
            builder.add_thread(pid=1, tid=i, name=f"thread_{i}")
        # The 256th thread must raise ValueError
        with self.assertRaises(ValueError):
            builder.add_thread(pid=1, tid=256, name="thread_256")

    def test_from_json_dict_and_string(self) -> None:
        json_data = {
            "traceEvents": [
                {
                    "ph": "M",
                    "name": "process_name",
                    "pid": 50,
                    "args": {"name": "p_json"},
                },
                {
                    "ph": "M",
                    "name": "thread_name",
                    "pid": 50,
                    "tid": 51,
                    "args": {"name": "t_json"},
                },
                {
                    "ph": "X",
                    "cat": "json_cat",
                    "name": "json_slice",
                    "ts": 10.0,
                    "dur": 5.0,
                    "pid": 50,
                    "tid": 51,
                    "args": {"k": "v"},
                },
                {
                    "ph": "I",
                    "cat": "json_cat",
                    "name": "json_inst",
                    "ts": 12.0,
                    "s": "p",
                    "pid": 50,
                    "tid": 51,
                    "args": {},
                },
            ]
        }
        builder_from_dict = fxt.Builder.from_json_dict(json_data)
        model_dict = create_model_from_fxt_builder(builder_from_dict)
        events_dict = list(model_dict.all_events())
        self.assertEqual(len(events_dict), 2)
        names = [e.name for e in events_dict]
        self.assertIn("json_slice", names)
        self.assertIn("json_inst", names)

        # Also test from_json_string
        import json

        json_str = json.dumps(json_data)
        builder_from_str = fxt.Builder.from_json_string(json_str)
        model_str = create_model_from_fxt_builder(builder_from_str)
        self.assertEqual(len(list(model_str.all_events())), 2)

    def test_write_to_file(self) -> None:
        builder = fxt.Builder()
        builder.add_process(pid=1, name="p")
        # Test stream write
        stream = io.BytesIO()
        builder.write_to_stream(stream)
        self.assertEqual(stream.getvalue(), builder.to_bytes())

        # Test file path write
        with tempfile.TemporaryDirectory() as tmp_dir:
            temp_path = os.path.join(tmp_dir, "test.fxt")
            builder.write_to_file(temp_path)
            with open(temp_path, "rb") as f:
                self.assertEqual(f.read(), builder.to_bytes())


if __name__ == "__main__":
    unittest.main()
