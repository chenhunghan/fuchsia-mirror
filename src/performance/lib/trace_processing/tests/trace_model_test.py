#!/usr/bin/env fuchsia-vendored-python
# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for trace_model.py."""

import types
import typing
import unittest
from collections.abc import Sequence

import test_utils
import trace_processing.trace_model as trace_model
import trace_processing.trace_time as trace_time


# Helper for bypassing static type checking: mypy rejects item assignment on a
# read-only mapping, but these tests need to attempt it to check the runtime
# behavior.
def _set_item(mapping: typing.Any, key: typing.Any, value: typing.Any) -> None:
    mapping[key] = value


class TraceModelTest(unittest.TestCase):
    """Trace model tests"""

    def assert_args_not_modifiable(
        self, obj: trace_model.Event | trace_model.SchedulingRecord
    ) -> None:
        """Asserts that in-place modification of `obj.args` is rejected."""
        with self.assertRaises(TypeError):
            _set_item(obj.args, "k", "new")

    def assert_event_children(
        self,
        event: trace_model.DurationEvent,
        durations: tuple[trace_model.DurationEvent, ...],
        flows: tuple[trace_model.FlowEvent, ...],
    ) -> None:
        """Asserts that `event`'s child durations and flows match and are immutable tuples."""
        # `child_durations` and `child_flows` are meant to be immutable tuples
        # (reusing the empty singleton `()` when empty).
        self.assertIsInstance(event.child_durations, tuple)
        self.assertIsInstance(event.child_flows, tuple)
        self.assertEqual(event.child_durations, durations)
        self.assertEqual(event.child_flows, flows)

    def _make_duration(
        self,
        name: str = "dur",
        start: int = 0,
        duration: int = 10,
        parent: trace_model.DurationEvent | None = None,
        child_durations: Sequence[trace_model.DurationEvent] = (),
        child_flows: Sequence[trace_model.FlowEvent] = (),
    ) -> trace_model.DurationEvent:
        base = trace_model.Event(
            "cat", name, trace_time.TimePoint(start), 1, 2, None
        )
        return trace_model.DurationEvent(
            duration=trace_time.TimeDelta(duration),
            parent=parent,
            child_durations=child_durations,
            child_flows=child_flows,
            base=base,
        )

    def _make_flow(
        self,
        name: str = "flow",
        start: int = 0,
        flow_id: str = "f1",
        phase: trace_model.FlowEventPhase = trace_model.FlowEventPhase.START,
        enclosing_duration: trace_model.DurationEvent | None = None,
        previous_flow: trace_model.FlowEvent | None = None,
        next_flow: trace_model.FlowEvent | None = None,
    ) -> trace_model.FlowEvent:
        base = trace_model.Event(
            "cat", name, trace_time.TimePoint(start), 1, 2, None
        )
        return trace_model.FlowEvent(
            id=flow_id,
            phase=phase,
            enclosing_duration=enclosing_duration,
            previous_flow=previous_flow,
            next_flow=next_flow,
            base=base,
        )

    def _make_model(self, events: list[trace_model.Event]) -> trace_model.Model:
        thread = trace_model.Thread(2, "t", events)
        proc = trace_model.Process(1, "p", [thread])
        return trace_model.Model([proc], {})

    def test_trace_slicing(self) -> None:
        model: trace_model.Model = test_utils.get_test_model()

        sliced_model: trace_model.Model = model.slice(
            trace_time.TimePoint.from_epoch_delta(
                trace_time.TimeDelta.from_microseconds(697800000)
            ),
            trace_time.TimePoint.from_epoch_delta(
                trace_time.TimeDelta.from_microseconds(698600000)
            ),
        )
        self.assertEqual(len(list(sliced_model.all_events())), 8)

        tail_model: trace_model.Model = model.slice(
            trace_time.TimePoint.from_epoch_delta(
                trace_time.TimeDelta.from_microseconds(697800000)
            ),
            None,
        )
        self.assertEqual(len(list(tail_model.all_events())), 9)

        head_model: trace_model.Model = model.slice(
            None,
            trace_time.TimePoint.from_epoch_delta(
                trace_time.TimeDelta.from_microseconds(698600000)
            ),
        )
        self.assertEqual(len(list(head_model.all_events())), 14)

        # Slicing with a doubly-infinite interval should result in an identical
        # model.
        copied_model: trace_model.Model = model.slice(None, None)
        test_utils.assertModelsEqual(self, model, copied_model)

    def test_assert_models_equal_scheduling_records_mismatch(self) -> None:
        model_a = test_utils.get_test_model()
        model_b = test_utils.get_test_model()
        # Drop one scheduling record from model_b on CPU 0.
        self.assertGreater(len(model_b.scheduling_records[0]), 1)
        sched_records = dict(model_b.scheduling_records)
        sched_records[0] = sched_records[0][:-1]
        model_b.scheduling_records = sched_records
        with self.assertRaises(AssertionError):
            test_utils.assertModelsEqual(self, model_a, model_b)

    def test_trace_slicing_sched(self) -> None:
        model: trace_model.Model = test_utils.get_test_model()

        sliced_model: trace_model.Model = model.slice(
            trace_time.TimePoint.from_epoch_delta(
                trace_time.TimeDelta.from_microseconds(697503118)
            ),
            trace_time.TimePoint.from_epoch_delta(
                trace_time.TimeDelta.from_microseconds(697778309)
            ),
        )
        self.assertEqual(len(sliced_model.scheduling_records[0]), 2)
        self.assertEqual(len(sliced_model.scheduling_records[1]), 0)

        tail_model: trace_model.Model = model.slice(
            trace_time.TimePoint.from_epoch_delta(
                trace_time.TimeDelta.from_microseconds(697778309)
            ),
            None,
        )
        self.assertEqual(len(tail_model.scheduling_records[0]), 3)
        self.assertEqual(len(tail_model.scheduling_records[1]), 2)

        head_model: trace_model.Model = model.slice(
            None,
            trace_time.TimePoint.from_epoch_delta(
                trace_time.TimeDelta.from_microseconds(697778309)
            ),
        )
        self.assertEqual(len(head_model.scheduling_records[0]), 2)
        self.assertEqual(len(head_model.scheduling_records[1]), 0)

    def test_slice_doesnt_reference_old_model(self) -> None:
        model: trace_model.Model = test_utils.get_test_model()
        sliced_model: trace_model.Model = model.slice(
            trace_time.TimePoint.from_epoch_delta(
                trace_time.TimeDelta.from_microseconds(697800000)
            ),
            trace_time.TimePoint.from_epoch_delta(
                trace_time.TimeDelta.from_microseconds(698600000)
            ),
        )
        self.assertEqual(len(list(sliced_model.all_events())), 8)

        sentinel_name: str = "OLD_MODEL_SENTINEL"
        for event in model.all_events():
            event.name = sentinel_name

        for event in sliced_model.all_events():
            if isinstance(event, trace_model.DurationEvent):
                for child_duration in event.child_durations:
                    self.assertIsNotNone(child_duration)
                    self.assertNotEqual(child_duration.name, sentinel_name)
                for child_flow in event.child_flows:
                    self.assertIsNotNone(child_flow)
                    self.assertNotEqual(child_flow.name, sentinel_name)
            elif isinstance(event, trace_model.FlowEvent):
                previous_flow_name: str | None = (
                    event.previous_flow.name
                    if event.previous_flow is not None
                    else None
                )
                next_flow_name: str | None = (
                    event.next_flow.name
                    if event.next_flow is not None
                    else None
                )
                self.assertNotEqual(previous_flow_name, sentinel_name)
                self.assertNotEqual(next_flow_name, sentinel_name)

    def test_event_args_immutable_mapping(self) -> None:
        e = trace_model.Event(
            "cat", "name", trace_time.TimePoint(0), 1, 2, None
        )
        self.assertEqual(e.args, {})
        self.assert_args_not_modifiable(e)

        # Updates via setter work and remain immutable.
        e.args = {"k": 1}
        self.assertEqual(e.args, {"k": 1})
        self.assert_args_not_modifiable(e)
        e.args = {}
        self.assertEqual(e.args, {})
        self.assert_args_not_modifiable(e)

    def test_event_args_share_caller_mapping(self) -> None:
        # Event wraps args without copying them, so events built from the same
        # dict share it rather than each holding a copy. The FXT importer
        # relies on this to avoid duplicating args across events that share an
        # arg set.
        src = {"a": 1}
        t0 = trace_time.TimePoint(0)
        e1 = trace_model.Event("cat", "e1", t0, 1, 2, src)
        e2 = trace_model.Event("cat", "e2", t0, 1, 2, src)
        src["b"] = 2
        self.assertEqual(e1.args, {"a": 1, "b": 2})
        self.assertEqual(e2.args, {"a": 1, "b": 2})
        self.assert_args_not_modifiable(e1)

    def test_event_subclass_construction_uses_base_args(self) -> None:
        t0 = trace_time.TimePoint(0)
        d0 = trace_time.TimeDelta(0)
        for base_args in (None, {"k": "v"}):
            base = trace_model.Event("cat", "name", t0, 1, 2, base_args)
            subclass_events: list[trace_model.Event] = [
                trace_model.InstantEvent(
                    trace_model.InstantEventScope.GLOBAL, base
                ),
                trace_model.CounterEvent(1, base),
                trace_model.DurationEvent(d0, None, [], [], base),
                trace_model.AsyncEvent(1, d0, base),
                trace_model.FlowEvent(
                    "f1",
                    trace_model.FlowEventPhase.START,
                    None,
                    None,
                    None,
                    base,
                ),
            ]
            for event in subclass_events:
                with self.subTest(cls=type(event).__name__, args=base_args):
                    self.assertEqual(event.args, base_args or {})
                    self.assertIs(event.args, base.args)

    def test_scheduling_record_args_immutable_mapping(self) -> None:
        t0 = trace_time.TimePoint(0)
        factories: list[
            tuple[str, typing.Callable[..., trace_model.SchedulingRecord]]
        ] = [
            (
                "ContextSwitch",
                lambda *a: trace_model.ContextSwitch(
                    t0,
                    1,
                    2,
                    10,
                    20,
                    trace_model.ThreadState.ZX_THREAD_STATE_RUNNING,
                    *a,
                ),
            ),
            (
                "Waking",
                lambda *a: trace_model.Waking(t0, 1, 10, *a),
            ),
        ]
        for name, make in factories:
            with self.subTest(cls=name):
                # Constructed with args={}.
                rec_empty = make({})
                self.assertEqual(rec_empty.args, {})
                self.assert_args_not_modifiable(rec_empty)

                # Constructed with args omitted.
                rec_default = make()
                self.assertEqual(rec_default.args, {})
                self.assert_args_not_modifiable(rec_default)

                # Constructed with a dict: isolated from caller mutations.
                src = {"a": 1}
                rec_with_src = make(src)
                self.assertEqual(rec_with_src.args, src)
                src["b"] = 2
                self.assertEqual(rec_with_src.args, {"a": 1})
                self.assert_args_not_modifiable(rec_with_src)

                # Constructed with a read-only view of a dict: still isolated
                # from mutations to the underlying dict.
                viewed_src = {"a": 1}
                rec_with_view = make(types.MappingProxyType(viewed_src))
                viewed_src["b"] = 2
                self.assertEqual(rec_with_view.args, {"a": 1})

                # Updates via setter.
                rec_empty.args = {"k": 1}
                self.assertEqual(rec_empty.args, {"k": 1})
                self.assert_args_not_modifiable(rec_empty)
                rec_empty.args = {}
                self.assertEqual(rec_empty.args, {})

    def test_slice_preserves_args(self) -> None:
        t0 = trace_time.TimePoint(0)
        t1 = trace_time.TimePoint(100)
        t2 = trace_time.TimePoint(200)
        e_empty = trace_model.Event("cat", "e_empty", t1, 1, 2, None)
        e_with_args = trace_model.Event("cat", "e_args", t1, 1, 2, {"k": "v"})
        model = self._make_model([e_empty, e_with_args])

        sliced = model.slice(t0, t2)

        sliced_events = list(sliced.all_events())
        sliced_empty = sliced_events[0]
        sliced_with_args = sliced_events[1]

        self.assertEqual(e_empty.args, {})
        self.assertEqual(sliced_empty.args, {})
        self.assertEqual(e_with_args.args, {"k": "v"})
        self.assertEqual(sliced_with_args.args, {"k": "v"})

        self.assert_args_not_modifiable(sliced_empty)
        self.assert_args_not_modifiable(sliced_with_args)

    def test_duration_event_empty_children_and_flows(self) -> None:
        from_list = self._make_duration(child_durations=[], child_flows=[])
        from_tuple = self._make_duration(child_durations=(), child_flows=())
        from_dict = trace_model.DurationEvent.consume_dict(
            {"cat": "c", "name": "n", "ts": 0, "pid": 1, "tid": 2, "dur": 10}
        )

        for event in (from_list, from_tuple, from_dict):
            self.assert_event_children(event, (), ())

    def test_duration_event_init_copies_into_read_only_sequences(self) -> None:
        child1 = self._make_duration("c1")
        child2 = self._make_duration("c2")
        flow1 = self._make_flow("f1")
        flow2 = self._make_flow("f2")
        caller_children = [child1]
        caller_flows = [flow1]

        event = self._make_duration(
            child_durations=caller_children, child_flows=caller_flows
        )
        caller_children.append(child2)
        caller_flows.append(flow2)

        # These stay the same despite us having modified caller_children and
        # caller_flows.
        self.assert_event_children(event, (child1,), (flow1,))

    def test_duration_event_setters_replace_and_isolate_sequences(self) -> None:
        child1 = self._make_duration("c1")
        child2 = self._make_duration("c2")
        flow1 = self._make_flow("f1")
        flow2 = self._make_flow("f2")
        event = self._make_duration()
        caller_children = [child1]
        caller_flows = [flow1]

        event.child_durations = caller_children
        event.child_flows = caller_flows
        caller_children.append(child2)
        caller_flows.append(flow2)

        # These stay the same despite us having modified caller_children and
        # caller_flows.
        self.assert_event_children(event, (child1,), (flow1,))

        event.child_durations = []
        event.child_flows = []
        self.assert_event_children(event, (), ())

    def test_slice_filters_out_of_window_children_and_flows(self) -> None:
        # Make an example set of events where `parent` lies inside the
        # `[100, 200]` slice interval, `child_in` (`[110, 130]`) and `flow_in`
        # (`115`) also lie inside `[100, 200]`, and `child_out` (`[180, 220]`)
        # and `flow_out` (`210`) end outside `[100, 200]`.
        parent = self._make_duration("parent", start=100, duration=100)
        child_in = self._make_duration(
            "child_in", start=110, duration=20, parent=parent
        )
        child_out = self._make_duration(
            "child_out", start=180, duration=40, parent=parent
        )
        flow_in = self._make_flow(
            "flow_in", start=115, flow_id="f1", enclosing_duration=parent
        )
        flow_out = self._make_flow(
            "flow_out",
            start=210,
            flow_id="f1",
            phase=trace_model.FlowEventPhase.END,
            enclosing_duration=parent,
            previous_flow=flow_in,
        )
        flow_in.next_flow = flow_out
        parent.child_durations = [child_in, child_out]
        parent.child_flows = [flow_in, flow_out]
        model = self._make_model(
            [parent, child_in, child_out, flow_in, flow_out]
        )

        sliced = model.slice(
            trace_time.TimePoint(100), trace_time.TimePoint(200)
        )

        # Check that slice() kept only the in-range child and flow (and cleared
        # `flow_in.next_flow` since `flow_out` was excluded).
        expected_parent = self._make_duration("parent", start=100, duration=100)
        expected_child = self._make_duration(
            "child_in", start=110, duration=20, parent=expected_parent
        )
        expected_flow = self._make_flow(
            "flow_in",
            start=115,
            flow_id="f1",
            enclosing_duration=expected_parent,
        )
        expected_parent.child_durations = [expected_child]
        expected_parent.child_flows = [expected_flow]
        expected = self._make_model(
            [expected_parent, expected_child, expected_flow]
        )
        test_utils.assertModelsEqual(self, sliced, expected)

    def test_slice_clears_children_and_flows_when_all_excluded(self) -> None:
        parent = self._make_duration("parent", start=100, duration=50)
        # These end outside the `[100, 200]` range that we apply slice() to
        # below.
        child_out = self._make_duration(
            "child_out", start=180, duration=40, parent=parent
        )
        flow_out = self._make_flow(
            "flow_out", start=210, enclosing_duration=parent
        )
        parent.child_durations = [child_out]
        parent.child_flows = [flow_out]
        model = self._make_model([parent, child_out, flow_out])

        sliced = model.slice(
            trace_time.TimePoint(100), trace_time.TimePoint(200)
        )

        expected = self._make_model(
            [self._make_duration("parent", start=100, duration=50)]
        )
        test_utils.assertModelsEqual(self, sliced, expected)
        [sliced_parent] = list(sliced.all_events())
        assert isinstance(sliced_parent, trace_model.DurationEvent)
        self.assert_event_children(sliced_parent, (), ())

    def test_slice_clears_parent_and_enclosing_duration_when_parent_excluded(
        self,
    ) -> None:
        # We apply slice() to `[100, 200]`, which is strictly within
        # `parent_out`'s `[50, 250]` interval; slice() excludes (rather than
        # truncates) events that start before `start` or end after `end`, so
        # `parent_out` gets excluded while `child_in` and `flow_in` are kept.
        parent_out = self._make_duration("parent_out", start=50, duration=200)
        child_in = self._make_duration(
            "child_in", start=110, duration=20, parent=parent_out
        )
        flow_in = self._make_flow(
            "flow_in", start=115, enclosing_duration=parent_out
        )
        parent_out.child_durations = [child_in]
        parent_out.child_flows = [flow_in]
        model = self._make_model([parent_out, child_in, flow_in])

        sliced = model.slice(
            trace_time.TimePoint(100), trace_time.TimePoint(200)
        )

        expected = self._make_model(
            [
                self._make_duration("child_in", start=110, duration=20),
                self._make_flow("flow_in", start=115),
            ]
        )
        test_utils.assertModelsEqual(self, sliced, expected)
