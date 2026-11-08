#!/usr/bin/env fuchsia-vendored-python
# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for trace_utils.py."""

import unittest

import test_utils
from reporting import metrics
from trace_processing import trace_model, trace_time, trace_utils

# Boilerplate-busting constants:
U = metrics.Unit
TCR = metrics.TestCaseResult


class TraceUtilsTest(unittest.TestCase):
    """Trace utils tests"""

    def test_compute_stats(self) -> None:
        self.assertAlmostEqual(
            trace_utils.percentile(
                [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0], 25
            ),
            3.0,
        )
        self.assertAlmostEqual(
            trace_utils.percentile(
                [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0], 50
            ),
            5.0,
        )
        self.assertAlmostEqual(
            trace_utils.percentile(
                [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0], 75
            ),
            7.0,
        )
        self.assertAlmostEqual(
            trace_utils.percentile(
                [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0], 25
            ),
            3.25,
        )
        self.assertAlmostEqual(
            trace_utils.percentile(
                [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0], 50
            ),
            5.5,
        )
        self.assertAlmostEqual(
            trace_utils.percentile(
                [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0], 75
            ),
            7.75,
        )

    def test_filter_events(self) -> None:
        events: list[trace_model.Event] = [
            trace_model.DurationEvent(
                duration=None,
                parent=None,
                child_durations=[],
                child_flows=[],
                base=trace_model.Event(
                    category="cat_a",
                    name="name_a",
                    start=trace_time.TimePoint.from_epoch_delta(
                        trace_time.TimeDelta.from_microseconds(
                            697778328.2160872
                        )
                    ),
                    pid=7009,
                    tid=7022,
                    args={},
                ),
            ),
            trace_model.DurationEvent(
                duration=None,
                parent=None,
                child_durations=[],
                child_flows=[],
                base=trace_model.Event(
                    category="cat_b",
                    name="name_b",
                    start=trace_time.TimePoint.from_epoch_delta(
                        trace_time.TimeDelta.from_microseconds(
                            697778328.2160872
                        )
                    ),
                    pid=7009,
                    tid=7022,
                    args={},
                ),
            ),
        ]

        filtered = list(
            trace_utils.filter_events(
                iter(events),
                category="cat_a",
                name="name_a",
                type=trace_model.Event,
            )
        )
        self.assertEqual(filtered, [events[0]])

        filtered2 = list(
            trace_utils.filter_events(
                iter(events),
                category="cat_c",
                name="name_c",
                type=trace_model.Event,
            )
        )
        self.assertEqual(filtered2, [])

    def test_filter_events_with_name_collection(self) -> None:
        events: list[trace_model.Event] = [
            trace_model.DurationEvent(
                duration=None,
                parent=None,
                child_durations=[],
                child_flows=[],
                base=trace_model.Event(
                    category="cat_a",
                    name="name_a",
                    start=trace_time.TimePoint.from_epoch_delta(
                        trace_time.TimeDelta.from_microseconds(0)
                    ),
                    pid=7009,
                    tid=7022,
                    args={},
                ),
            ),
            trace_model.DurationEvent(
                duration=None,
                parent=None,
                child_durations=[],
                child_flows=[],
                base=trace_model.Event(
                    category="cat_b",
                    name="name_b",
                    start=trace_time.TimePoint.from_epoch_delta(
                        trace_time.TimeDelta.from_microseconds(0)
                    ),
                    pid=7009,
                    tid=7022,
                    args={},
                ),
            ),
            trace_model.DurationEvent(
                duration=None,
                parent=None,
                child_durations=[],
                child_flows=[],
                base=trace_model.Event(
                    category="cat_c",
                    name="name_c",
                    start=trace_time.TimePoint.from_epoch_delta(
                        trace_time.TimeDelta.from_microseconds(0)
                    ),
                    pid=7009,
                    tid=7022,
                    args={},
                ),
            ),
        ]

        filtered = list(
            trace_utils.filter_events(
                iter(events), name={"name_a", "name_c"}, type=trace_model.Event
            )
        )
        self.assertEqual(filtered, [events[0], events[2]])

        filtered2 = list(
            trace_utils.filter_events(
                iter(events), name=["name_b", "name_c"], type=trace_model.Event
            )
        )
        self.assertEqual(filtered2, [events[1], events[2]])

    def test_filter_events_with_type(self) -> None:
        events: list[trace_model.Event] = [
            trace_model.DurationEvent(
                duration=None,
                parent=None,
                child_durations=[],
                child_flows=[],
                base=trace_model.Event(
                    category="cat_a",
                    name="name_a",
                    start=trace_time.TimePoint.from_epoch_delta(
                        trace_time.TimeDelta.from_microseconds(
                            697778328.2160872
                        )
                    ),
                    pid=7009,
                    tid=7022,
                    args={},
                ),
            ),
            trace_model.DurationEvent(
                duration=None,
                parent=None,
                child_durations=[],
                child_flows=[],
                base=trace_model.Event(
                    category="cat_b",
                    name="name_b",
                    start=trace_time.TimePoint.from_epoch_delta(
                        trace_time.TimeDelta.from_microseconds(
                            697778328.2160872
                        )
                    ),
                    pid=7009,
                    tid=7022,
                    args={},
                ),
            ),
        ]

        filtered: list[trace_model.Event] = list(
            trace_utils.filter_events(
                iter(events),
                category="cat_a",
                name="name_a",
                type=trace_model.DurationEvent,
            )
        )
        self.assertEqual(filtered, [events[0]])

        filtered2: list[trace_model.Event] = list(
            trace_utils.filter_events(
                iter(events),
                category="cat_c",
                name="name_c",
                type=trace_model.DurationEvent,
            )
        )
        self.assertEqual(filtered2, [])

        filtered3: list[trace_model.Event] = list(
            trace_utils.filter_events(
                iter(events),
                category="cat_a",
                name="name_a",
                type=trace_model.InstantEvent,
            )
        )
        self.assertEqual(filtered3, [])

    def test_total_event_duration(self) -> None:
        model: trace_model.Model = test_utils.get_test_model()
        mode_duration: trace_time.TimeDelta = (
            trace_time.TimeDelta.from_microseconds(
                test_utils.TEST_MODEL_END_TIME_IN_US
            )
            - trace_time.TimeDelta.from_microseconds(
                test_utils.TEST_MODEL_BEGIN_TIME_IN_US
            )
        )
        self.assertEqual(
            trace_utils.total_event_duration(model.all_events()), mode_duration
        )

    def test_standard_metrics_set(self) -> None:
        values: list[float] = [0.0, 10.0]
        unit = U.milliseconds

        results = trace_utils.standard_metrics_set(
            values=values, label_prefix="Foo", unit=unit
        )
        p_format_str = "{}, {}th percentile"
        self.maxDiff = None
        self.assertEqual(
            results,
            [
                TCR("FooP5", unit, [0.5], p_format_str.format("Foo", 5)),
                TCR("FooP25", unit, [2.5], p_format_str.format("Foo", 25)),
                TCR("FooP50", unit, [5.0], p_format_str.format("Foo", 50)),
                TCR("FooP75", unit, [7.5], p_format_str.format("Foo", 75)),
                TCR("FooP95", unit, [9.5], p_format_str.format("Foo", 95)),
                TCR("FooMin", unit, [0.0], "Foo, minimum"),
                TCR("FooMax", unit, [10.0], "Foo, maximum"),
                TCR("FooAverage", unit, [5.0], "Foo, mean"),
            ],
        )

        results = trace_utils.standard_metrics_set(
            values=values, label_prefix="Foo", unit=unit, doc_prefix="Foo doc"
        )
        self.assertEqual(
            results,
            [
                TCR("FooP5", unit, [0.5], p_format_str.format("Foo doc", 5)),
                TCR("FooP25", unit, [2.5], p_format_str.format("Foo doc", 25)),
                TCR("FooP50", unit, [5.0], p_format_str.format("Foo doc", 50)),
                TCR("FooP75", unit, [7.5], p_format_str.format("Foo doc", 75)),
                TCR("FooP95", unit, [9.5], p_format_str.format("Foo doc", 95)),
                TCR("FooMin", unit, [0.0], "Foo doc, minimum"),
                TCR("FooMax", unit, [10.0], "Foo doc, maximum"),
                TCR("FooAverage", unit, [5.0], "Foo doc, mean"),
            ],
        )

    def test_common_process_start_time_and_min_start_flows(self) -> None:
        # Process 1 starts tracing at t=10us; Process 2 starts tracing later at t=50us.
        # Flow 0 (t=20us -> t=60us) starts before Process 2 began recording;
        # Flow 1 (t=70us -> t=90us) occurs after both processes are recording.
        p1_early_dur = trace_model.DurationEvent(
            duration=trace_time.TimeDelta.from_microseconds(5),
            parent=None,
            child_durations=[],
            child_flows=[],
            base=trace_model.Event(
                category="gfx",
                name="StartFrame",
                start=trace_time.TimePoint.from_microseconds(20),
                pid=1,
                tid=10,
                args={},
            ),
        )
        p2_first_event = trace_model.DurationEvent(
            duration=trace_time.TimeDelta.from_microseconds(5),
            parent=None,
            child_durations=[],
            child_flows=[],
            base=trace_model.Event(
                category="gfx",
                name="Init",
                start=trace_time.TimePoint.from_microseconds(50),
                pid=2,
                tid=20,
                args={},
            ),
        )
        p2_early_vsync = trace_model.DurationEvent(
            duration=trace_time.TimeDelta.from_microseconds(5),
            parent=None,
            child_durations=[],
            child_flows=[],
            base=trace_model.Event(
                category="gfx",
                name="Vsync",
                start=trace_time.TimePoint.from_microseconds(60),
                pid=2,
                tid=20,
                args={},
            ),
        )
        f0_end = trace_model.FlowEvent(
            id="0",
            phase=trace_model.FlowEventPhase.END,
            enclosing_duration=p2_early_vsync,
            previous_flow=None,
            next_flow=None,
            base=trace_model.Event(
                category="gfx",
                name="frame_flow",
                start=trace_time.TimePoint.from_microseconds(60),
                pid=2,
                tid=20,
                args={},
            ),
        )
        f0_start = trace_model.FlowEvent(
            id="0",
            phase=trace_model.FlowEventPhase.START,
            enclosing_duration=p1_early_dur,
            previous_flow=None,
            next_flow=f0_end,
            base=trace_model.Event(
                category="gfx",
                name="frame_flow",
                start=trace_time.TimePoint.from_microseconds(20),
                pid=1,
                tid=10,
                args={},
            ),
        )
        f0_end.previous_flow = f0_start
        p1_early_dur.child_flows = (f0_start,)
        p2_early_vsync.child_flows = (f0_end,)

        p1_valid_dur = trace_model.DurationEvent(
            duration=trace_time.TimeDelta.from_microseconds(5),
            parent=None,
            child_durations=(),
            child_flows=(),
            base=trace_model.Event(
                category="gfx",
                name="StartFrame",
                start=trace_time.TimePoint.from_microseconds(70),
                pid=1,
                tid=10,
                args={},
            ),
        )
        p2_valid_vsync = trace_model.DurationEvent(
            duration=trace_time.TimeDelta.from_microseconds(5),
            parent=None,
            child_durations=(),
            child_flows=(),
            base=trace_model.Event(
                category="gfx",
                name="Vsync",
                start=trace_time.TimePoint.from_microseconds(90),
                pid=2,
                tid=20,
                args={},
            ),
        )
        f1_end = trace_model.FlowEvent(
            id="1",
            phase=trace_model.FlowEventPhase.END,
            enclosing_duration=p2_valid_vsync,
            previous_flow=None,
            next_flow=None,
            base=trace_model.Event(
                category="gfx",
                name="frame_flow",
                start=trace_time.TimePoint.from_microseconds(90),
                pid=2,
                tid=20,
                args={},
            ),
        )
        f1_start = trace_model.FlowEvent(
            id="1",
            phase=trace_model.FlowEventPhase.START,
            enclosing_duration=p1_valid_dur,
            previous_flow=None,
            next_flow=f1_end,
            base=trace_model.Event(
                category="gfx",
                name="frame_flow",
                start=trace_time.TimePoint.from_microseconds(70),
                pid=1,
                tid=10,
                args={},
            ),
        )
        f1_end.previous_flow = f1_start
        p1_valid_dur.child_flows = (f1_start,)
        p2_valid_vsync.child_flows = (f1_end,)

        model = trace_model.Model(
            processes=[
                trace_model.Process(
                    1,
                    "scenic",
                    [
                        trace_model.Thread(
                            10,
                            "main",
                            [p1_early_dur, f0_start, p1_valid_dur, f1_start],
                        )
                    ],
                ),
                trace_model.Process(
                    2,
                    "display",
                    [
                        trace_model.Thread(
                            20,
                            "vsync",
                            [
                                p2_first_event,
                                p2_early_vsync,
                                f0_end,
                                p2_valid_vsync,
                                f1_end,
                            ],
                        )
                    ],
                ),
            ],
            scheduling_records={},
        )

        common_start = trace_utils.get_common_process_start_time(
            model,
            "StartFrame",
            category="gfx",
            type=trace_model.DurationEvent,
        )
        self.assertEqual(
            common_start, trace_time.TimePoint.from_microseconds(50)
        )

        # Early flow starting before common_start is ignored when min_start is set.
        self.assertEqual(
            list(
                trace_utils.get_following_flow_events(
                    p1_early_dur, min_start=common_start
                )
            ),
            [],
        )
        self.assertIsNone(
            trace_utils.get_nearest_following_flow_event(
                p1_early_dur, "gfx", "Vsync", min_start=common_start
            )
        )

        # Valid flow starting after common_start resolves identically to adjust_to_common_process_start.
        nearest = trace_utils.get_nearest_following_flow_event(
            p1_valid_dur, "gfx", "Vsync", min_start=common_start
        )
        self.assertIs(nearest, p2_valid_vsync)

        sliced_model = trace_utils.adjust_to_common_process_start(
            model,
            "StartFrame",
            category="gfx",
            type=trace_model.DurationEvent,
        )
        sliced_starts = list(
            trace_utils.filter_events(
                sliced_model.all_events(),
                category="gfx",
                name="StartFrame",
                type=trace_model.DurationEvent,
            )
        )
        self.assertEqual(len(sliced_starts), 1)
        self.assertEqual(sliced_starts[0].start, p1_valid_dur.start)
